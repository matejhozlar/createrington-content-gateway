use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::value::RawValue;
use utoipa::ToSchema;

use crate::{
    app::AppState,
    auth::Pool,
    batch,
    cache::{Answer, AnswerCache, Entry, Finisher, LoadFailure, Outcome, Waiting},
    error::ApiError,
    passthrough::{self, DAY_MS, Freshness, HOUR_MS, MINUTE_MS, fresh},
    upstream::{Source, UpstreamFailure},
};

pub const MODRINTH_API: &str = "https://api.modrinth.com/v2";
pub const TIMEOUT: Duration = Duration::from_secs(10);
/// Modrinth asks every client to say who it is; this is what the gateway says unless `MODRINTH_USER_AGENT` is set.
pub const DEFAULT_USER_AGENT: &str = concat!("content-gateway/", env!("CARGO_PKG_VERSION"));
/// How many hashes one `POST /v1/modrinth/version_files` may ask for.
pub const MOST_HASHES: usize = 1000;
const HASHES_PER_CALL: usize = 100;
const FOUND_MS: u64 = HOUR_MS;
const NOT_FOUND_MS: u64 = 10 * MINUTE_MS;
const SHA1_LENGTH: usize = 40;

/// The cache table of the Modrinth routes.
pub fn freshness(segments: &[&str]) -> Option<Freshness> {
    match segments {
        ["search"] => fresh(5 * MINUTE_MS, None),
        ["tag", _] => fresh(DAY_MS, None),
        ["project", _] | ["project", _, "check"] => fresh(HOUR_MS, Some(5 * MINUTE_MS)),
        ["projects"] | ["versions"] => fresh(10 * MINUTE_MS, None),
        ["project", _, "version"] | ["project", _, "dependencies"] | ["user", _, "projects"] => {
            fresh(10 * MINUTE_MS, None)
        }
        ["project", _, "members"] | ["team", _, "members"] | ["user", _] => {
            fresh(HOUR_MS, Some(5 * MINUTE_MS))
        }
        ["version", _] => fresh(10 * MINUTE_MS, Some(10 * MINUTE_MS)),
        _ => None,
    }
}

/// Whether a `version_file/{hash}` query asks for SHA-1, which is the lookup the gateway keeps per hash.
pub fn sha1_query(query: Option<&str>) -> bool {
    match query.filter(|query| !query.is_empty()) {
        None => true,
        Some(query) => query
            .split('&')
            .all(|pair| pair.eq_ignore_ascii_case("algorithm=sha1")),
    }
}

/// Whether a `version_files` body asks for SHA-1 (or nothing), the lookup the gateway keeps per hash. Any other
/// body goes to Modrinth as it came.
pub fn sha1_body(body: &[u8]) -> bool {
    #[derive(Deserialize)]
    struct Algorithm {
        algorithm: Option<String>,
    }
    serde_json::from_slice::<Algorithm>(body).map_or(true, |asked| {
        asked
            .algorithm
            .is_none_or(|algorithm| algorithm.eq_ignore_ascii_case("sha1"))
    })
}

/// The body of `POST /v1/modrinth/version_files`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct VersionFilesRequest {
    /// SHA-1 hashes of the wanted files, 40 hex digits each, up to 1000.
    #[schema(example = json!(["3e1dd9fb5ea2f0dd4ffcc2b1a7d0bb1a1a9d6a8e"]))]
    pub hashes: Vec<String>,
    /// Only `sha1` is supported.
    #[serde(default = "sha1")]
    #[schema(example = "sha1")]
    pub algorithm: String,
}

fn sha1() -> String {
    "sha1".to_owned()
}

/// What Modrinth knows about the file with one SHA-1: its version as Modrinth sent it, or nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Known {
    pub hash: String,
    pub version: Option<Bytes>,
}

/// The versions of the kept hashes, and whether Modrinth was asked, found as `hit`, `miss` or `stale`.
pub struct Looked {
    pub known: Vec<Known>,
    pub cache: &'static str,
}

enum Slot {
    Kept(Arc<Entry>),
    Waiting { waiting: Waiting, started: bool },
}

struct Mine {
    hash: String,
    load_key: String,
    finisher: Finisher,
}

fn key(hash: &str) -> String {
    format!("modrinth:sha1:{hash}")
}

/// A lowercase SHA-1, or none when the text is not one.
pub fn sha1_of(text: &str) -> Option<String> {
    (text.len() == SHA1_LENGTH && text.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| text.to_ascii_lowercase())
}

fn usable(entry: &Entry) -> bool {
    entry.answer.status == StatusCode::NOT_FOUND || entry.row.is_some()
}

/// Modrinth's version for each lowercase SHA-1, from the cache while fresh. The hashes not kept are asked for in one
/// call per 100, drawn from the Modrinth budget of `pool` and joined with any call for the same hash already in
/// flight. A hash Modrinth does not know is kept as such for 10 minutes, a found version for an hour. Kept answers
/// past their fresh time are given when Modrinth fails.
pub async fn lookup(
    state: &Arc<AppState>,
    pool: &Pool,
    hashes: Vec<String>,
    forced: bool,
) -> Result<Looked, LoadFailure> {
    let keys: Vec<String> = hashes.iter().map(|hash| key(hash)).collect();
    let found = state.cache.lookup_many(&keys, state.clock.now_ms()).await;
    let now = state.clock.now_ms();
    let mut slots = Vec::with_capacity(hashes.len());
    let mut mine = Vec::new();
    for ((hash, key), kept) in hashes.into_iter().zip(keys).zip(found) {
        let kept = kept.filter(|entry| usable(entry));
        if !forced && let Some(entry) = kept.clone().filter(|entry| entry.is_fresh(now)) {
            slots.push((hash, kept, Slot::Kept(entry)));
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
                hash: hash.clone(),
                load_key,
                finisher,
            });
        }
        slots.push((hash, kept, Slot::Waiting { waiting, started }));
    }

    if !mine.is_empty() {
        tokio::spawn(fetch(state.clone(), pool.clone(), mine, forced));
    }

    let mut known = Vec::with_capacity(slots.len());
    let mut stale = false;
    let mut called = false;
    for (hash, kept, slot) in slots {
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
        let entry = match loaded {
            Ok(entry) if usable(&entry) => entry,
            Ok(_) => return Err(LoadFailure::Upstream(UpstreamFailure::Unreadable)),
            Err(failure) => {
                let now = state.clock.now_ms();
                match kept.filter(|entry| entry.is_stale(now)) {
                    Some(entry) if batch::failed_upstream(&failure) => {
                        stale = true;
                        entry
                    }
                    _ => return Err(failure),
                }
            }
        };
        known.push(Known {
            hash,
            version: entry.row.clone(),
        });
    }
    if !called {
        state.modrinth_budget.record_hit(&pool.consumer, &pool.name);
    }
    Ok(Looked {
        known,
        cache: if stale {
            "stale"
        } else if called {
            "miss"
        } else {
            "hit"
        },
    })
}

async fn fetch(state: Arc<AppState>, pool: Pool, mine: Vec<Mine>, forced: bool) {
    let mut missing = Vec::with_capacity(mine.len());
    for load in mine {
        let kept = state
            .cache
            .get(&key(&load.hash))
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
    let calls = u32::try_from(missing.len().div_ceil(HASHES_PER_CALL)).unwrap_or(u32::MAX);
    if let Err(refusal) = state
        .modrinth_budget
        .admit(&pool.consumer, &pool.name, calls)
        .await
    {
        for load in missing {
            let outcome = Outcome {
                loaded: Err(LoadFailure::Refused(refusal)),
                called: false,
            };
            state.cache.finish(&load.load_key, load.finisher, outcome);
        }
        return;
    }
    let mut missing = missing.into_iter().peekable();
    while missing.peek().is_some() {
        let chunk: Vec<Mine> = missing.by_ref().take(HASHES_PER_CALL).collect();
        tokio::spawn(fetch_chunk(state.clone(), pool.clone(), chunk));
    }
}

async fn fetch_chunk(state: Arc<AppState>, pool: Pool, chunk: Vec<Mine>) {
    let hashes: Vec<&str> = chunk.iter().map(|load| load.hash.as_str()).collect();
    let body = serde_json::json!({ "hashes": hashes, "algorithm": "sha1" }).to_string();
    let sent = passthrough::send_recorded(
        &state,
        Source::Modrinth,
        &pool,
        Method::POST,
        "version_files",
        None,
        body.into(),
    )
    .await;
    let found = match sent {
        Ok(answer) if answer.status == StatusCode::OK => match split(&answer) {
            Some(found) => Ok(found),
            None => Err(LoadFailure::Upstream(UpstreamFailure::Unreadable)),
        },
        Ok(answer) => Err(LoadFailure::Answered(answer)),
        Err(failure) => Err(failure),
    };

    let now = state.clock.now_ms();
    for load in chunk {
        let loaded = match &found {
            Ok(versions) => {
                let entry = match versions.get(&load.hash) {
                    Some(version) => {
                        let mut entry = Entry::new(
                            Answer {
                                status: StatusCode::OK,
                                content_type: Some(json()),
                                body: version.clone(),
                            },
                            now,
                            FOUND_MS,
                        );
                        entry.row = Some(version.clone());
                        entry
                    }
                    None => Entry::new(
                        Answer {
                            status: StatusCode::NOT_FOUND,
                            content_type: None,
                            body: Bytes::new(),
                        },
                        now,
                        NOT_FOUND_MS,
                    ),
                };
                let entry = Arc::new(entry);
                state.cache.insert(&key(&load.hash), entry.clone());
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

fn split(answer: &Answer) -> Option<HashMap<String, Bytes>> {
    let versions: HashMap<String, &RawValue> = serde_json::from_slice(&answer.body).ok()?;
    Some(
        versions
            .into_iter()
            .map(|(hash, version)| {
                (
                    hash.to_ascii_lowercase(),
                    answer.body.slice_ref(version.get().as_bytes()),
                )
            })
            .collect(),
    )
}

fn json() -> HeaderValue {
    HeaderValue::from_static("application/json; charset=utf-8")
}

/// What `GET api.modrinth.com/v2/{path}` answers, from the cache while it is fresh.
///
/// The query string goes to Modrinth as it came, and the answer comes back with Modrinth's status, content type and
/// body, marked `X-Cache`. The routes of the cache table are kept for their fresh time; every other route is asked
/// each time. `version_file/{sha1}` is kept per hash and shared with `POST version_files`. `Cache-Control: no-cache`
/// asks Modrinth again. Modrinth needs no key but limits calls per address, so every call counts against the
/// Modrinth budget of the request's pool, and a 429 holds every pool off.
#[utoipa::path(
    get,
    operation_id = "modrinth_get",
    path = "/v1/modrinth/{path}",
    tag = "modrinth",
    security(("bearer" = [])),
    params(
        ("path" = String, Path, description = "The path under `api.modrinth.com/v2/`, such as `search`, `project/sodium` or `version_file/{sha1}`: segments of letters, digits, `-` and `_`", example = "search"),
        ("X-Budget-Pool" = Option<String>, Header, description = "The pool the call draws from. Required when the token has more than one pool"),
        ("Cache-Control" = Option<String>, Header, description = "`no-cache` asks Modrinth again instead of answering from the cache"),
    ),
    responses(
        (status = 200, description = "Modrinth's answer, as it came", body = Value, headers(("X-Cache" = String, description = "`hit` from the cache, `miss` from Modrinth, `stale` an old answer given because Modrinth could not be asked"))),
        (status = 400, description = "`INVALID_PATH`, `INVALID_HASH`, `POOL_REQUIRED` or `UNKNOWN_POOL`", body = ApiError),
        (status = 401, description = "No valid consumer token", body = ApiError),
        (status = 502, description = "Modrinth could not be reached or answered something the gateway cannot keep: `UPSTREAM_UNREACHABLE`, `UPSTREAM_TOO_LARGE` or `UPSTREAM_UNREADABLE`", body = ApiError),
        (status = 503, description = "The call was refused: `BUDGET_SPENT` this minute or `UPSTREAM_HOLD` after a 429 from Modrinth", body = ApiError, headers(("Retry-After" = u64, description = "Seconds until the next call can go out"))),
        (status = 504, description = "Modrinth did not answer in time, `UPSTREAM_TIMEOUT`", body = ApiError),
        (status = "default", description = "Any other status Modrinth answered, with its body. Only the gateway's own answers lack `X-Cache`"),
    )
)]
pub async fn passthrough_get(
    State(state): State<Arc<AppState>>,
    Path(path): Path<String>,
    pool: Pool,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    passthrough::passthrough(
        state,
        Source::Modrinth,
        passthrough::Request {
            path,
            pool,
            method: Method::GET,
            uri,
            headers,
            body: Bytes::new(),
        },
    )
    .await
}

/// What `POST api.modrinth.com/v2/{path}` answers to this body.
///
/// `POST version_files` with `{"hashes": [...], "algorithm": "sha1"}` is answered per hash from the cache, keyed by
/// lowercase hash with unknown hashes left out as Modrinth does, and only the hashes not kept are asked for, 100 per
/// call, at most 1000 hashes. Every other body goes to Modrinth each time.
#[utoipa::path(
    post,
    operation_id = "modrinth_post",
    path = "/v1/modrinth/{path}",
    tag = "modrinth",
    security(("bearer" = [])),
    params(
        ("path" = String, Path, description = "The path under `api.modrinth.com/v2/`, such as `version_files` or `version_files/update`: segments of letters, digits, `-` and `_`", example = "version_files"),
        ("X-Budget-Pool" = Option<String>, Header, description = "The pool the call draws from. Required when the token has more than one pool"),
        ("Cache-Control" = Option<String>, Header, description = "`no-cache` asks Modrinth again instead of answering from the cache"),
    ),
    request_body(content = VersionFilesRequest, description = "The JSON body Modrinth expects at this path; this shape for `version_files`", content_type = "application/json"),
    responses(
        (status = 200, description = "Modrinth's answer, as it came, or the versions found keyed by hash for `version_files`", body = Value, headers(("X-Cache" = String, description = "`hit` when every hash was kept, `miss` when Modrinth was asked, `stale` an old answer given because Modrinth could not be asked"))),
        (status = 400, description = "`INVALID_PATH`, `INVALID_BODY` (no hashes, more than 1000, or another algorithm), `INVALID_HASH`, `POOL_REQUIRED` or `UNKNOWN_POOL`", body = ApiError),
        (status = 401, description = "No valid consumer token", body = ApiError),
        (status = 413, description = "The hashes need more calls than a whole minute allows, `REQUEST_TOO_LARGE`; split them", body = ApiError),
        (status = 502, description = "Modrinth could not be reached or answered something the gateway cannot keep: `UPSTREAM_UNREACHABLE`, `UPSTREAM_TOO_LARGE` or `UPSTREAM_UNREADABLE`", body = ApiError),
        (status = 503, description = "The call was refused: `BUDGET_SPENT` this minute or `UPSTREAM_HOLD` after a 429 from Modrinth", body = ApiError, headers(("Retry-After" = u64, description = "Seconds until the next call can go out"))),
        (status = 504, description = "Modrinth did not answer in time, `UPSTREAM_TIMEOUT`", body = ApiError),
        (status = "default", description = "Any other status Modrinth answered, with its body. Only the gateway's own answers lack `X-Cache`"),
    )
)]
pub async fn passthrough_post(
    State(state): State<Arc<AppState>>,
    Path(path): Path<String>,
    pool: Pool,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    passthrough::passthrough(
        state,
        Source::Modrinth,
        passthrough::Request {
            path,
            pool,
            method: Method::POST,
            uri,
            headers,
            body,
        },
    )
    .await
}

/// The answer to `GET version_file/{hash}` from the per-hash cache.
pub async fn version_file(state: Arc<AppState>, pool: Pool, hash: &str, forced: bool) -> Response {
    let Some(hash) = sha1_of(hash) else {
        return ApiError::invalid_hash().into_response();
    };
    let looked = match lookup(&state, &pool, vec![hash], forced).await {
        Ok(looked) => looked,
        Err(failure) => return passthrough::failed(failure, &pool, Source::Modrinth),
    };
    match looked
        .known
        .into_iter()
        .next()
        .and_then(|known| known.version)
    {
        Some(version) => passthrough::respond(
            &Answer {
                status: StatusCode::OK,
                content_type: Some(json()),
                body: version,
            },
            looked.cache,
        ),
        None => passthrough::respond(
            &Answer {
                status: StatusCode::NOT_FOUND,
                content_type: None,
                body: Bytes::new(),
            },
            looked.cache,
        ),
    }
}

/// The answer to `POST version_files` from the per-hash cache: the versions found, keyed by lowercase hash.
pub async fn version_files(
    state: Arc<AppState>,
    pool: Pool,
    body: &[u8],
    forced: bool,
) -> Response {
    let Ok(request) = serde_json::from_slice::<VersionFilesRequest>(body) else {
        return ApiError::invalid_body("a JSON object with `hashes`, a list of SHA-1 hashes")
            .into_response();
    };
    if !request.algorithm.eq_ignore_ascii_case("sha1") {
        return ApiError::invalid_body("only the `sha1` algorithm is supported").into_response();
    }
    if request.hashes.is_empty() {
        return ApiError::invalid_body("`hashes` must list at least one SHA-1").into_response();
    }
    let mut seen = HashSet::new();
    let mut hashes = Vec::new();
    for hash in &request.hashes {
        let Some(hash) = sha1_of(hash) else {
            return ApiError::invalid_hash().into_response();
        };
        if seen.insert(hash.clone()) {
            hashes.push(hash);
        }
        if hashes.len() > MOST_HASHES {
            return ApiError::invalid_body("`hashes` may list at most 1000 SHA-1s").into_response();
        }
    }
    let looked = match lookup(&state, &pool, hashes, forced).await {
        Ok(looked) => looked,
        Err(failure) => return passthrough::failed(failure, &pool, Source::Modrinth),
    };
    let found: Vec<&Known> = looked
        .known
        .iter()
        .filter(|known| known.version.is_some())
        .collect();
    let mut body = Vec::with_capacity(
        found
            .iter()
            .map(|known| known.version.as_ref().map_or(0, Bytes::len) + SHA1_LENGTH + 4)
            .sum::<usize>()
            + 2,
    );
    body.push(b'{');
    for (index, known) in found.iter().enumerate() {
        if index > 0 {
            body.push(b',');
        }
        body.push(b'"');
        body.extend_from_slice(known.hash.as_bytes());
        body.extend_from_slice(b"\":");
        if let Some(version) = &known.version {
            body.extend_from_slice(version);
        }
    }
    body.push(b'}');
    passthrough::respond(
        &Answer {
            status: StatusCode::OK,
            content_type: Some(json()),
            body: body.into(),
        },
        looked.cache,
    )
}

#[cfg(test)]
mod tests {
    use axum::http::Request;

    use super::*;
    use crate::test_support::*;

    const JEI: &str = "3e1dd9fb5ea2f0dd4ffcc2b1a7d0bb1a1a9d6a8e";
    const SODIUM: &str = "0123456789abcdef0123456789abcdef01234567";

    fn version(hash: &str, name: &str) -> String {
        format!(
            r#"{{"id":"v1","files":[{{"hashes":{{"sha1":"{hash}"}},"url":"https://cdn.modrinth.com/data/x/versions/y/{name}","filename":"{name}","size":10,"primary":true}}]}}"#
        )
    }

    #[tokio::test]
    async fn looks_a_hash_up_once_and_keeps_the_version() {
        let mock = Mock::start(Duration::ZERO, |_, _| {
            reply(200, format!(r#"{{"{JEI}":{}}}"#, version(JEI, "jei.jar")))
        })
        .await;
        let gateway = gateway(&mock, 300, TIMEOUT);

        let got = get(&gateway.router, &format!("/v1/modrinth/version_file/{JEI}")).await;
        assert_eq!(got.status, StatusCode::OK);
        assert_eq!(got.header("x-cache"), Some("miss"));
        assert_eq!(got.json()["files"][0]["filename"], "jei.jar");
        let seen = mock.seen();
        assert_eq!(seen[0].method, "POST");
        assert_eq!(seen[0].uri, "/v2/version_files");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&seen[0].body).unwrap(),
            serde_json::json!({ "hashes": [JEI], "algorithm": "sha1" })
        );
        assert_eq!(seen[0].user_agent.as_deref(), Some(USER_AGENT));
        assert_eq!(seen[0].key, None);

        let again = get(
            &gateway.router,
            &format!(
                "/v1/modrinth/version_file/{}?algorithm=sha1",
                JEI.to_ascii_uppercase()
            ),
        )
        .await;
        assert_eq!(again.header("x-cache"), Some("hit"));
        assert_eq!(mock.calls(), 1);
        assert_eq!(gateway.state.budget.snapshot().await.used, 0);
        let modrinth = gateway.state.modrinth_budget.snapshot().await;
        assert_eq!(modrinth.used, 1);
        assert_eq!(modrinth.days[0].pools[0].counts.calls, 1);
        assert_eq!(modrinth.days[0].pools[0].counts.hits, 1);
    }

    #[tokio::test]
    async fn a_hash_modrinth_does_not_know_answers_404_and_is_kept_ten_minutes() {
        let mock = Mock::start(Duration::ZERO, |_, _| reply(200, "{}")).await;
        let gateway = gateway(&mock, 300, TIMEOUT);

        let got = get(&gateway.router, &format!("/v1/modrinth/version_file/{JEI}")).await;
        assert_eq!(got.status, StatusCode::NOT_FOUND);
        assert_eq!(got.header("x-cache"), Some("miss"));
        get(&gateway.router, &format!("/v1/modrinth/version_file/{JEI}")).await;
        assert_eq!(mock.calls(), 1);

        gateway.clock.advance(NOT_FOUND_MS + 1);
        get(&gateway.router, &format!("/v1/modrinth/version_file/{JEI}")).await;
        assert_eq!(mock.calls(), 2);
    }

    #[tokio::test]
    async fn version_files_asks_only_for_the_hashes_not_kept_and_joins_the_rows() {
        let mock = Mock::start(Duration::ZERO, |_, seen| {
            let asked: serde_json::Value = serde_json::from_str(&seen.body).unwrap();
            let mut body = String::from("{");
            for hash in asked["hashes"].as_array().unwrap() {
                let hash = hash.as_str().unwrap();
                if hash == SODIUM {
                    continue;
                }
                if body.len() > 1 {
                    body.push(',');
                }
                body.push_str(&format!(r#""{hash}":{}"#, version(hash, "file.jar")));
            }
            body.push('}');
            reply(200, body)
        })
        .await;
        let gateway = gateway(&mock, 300, TIMEOUT);
        get(&gateway.router, &format!("/v1/modrinth/version_file/{JEI}")).await;

        let other = "abcdefabcdefabcdefabcdefabcdefabcdefabcd";
        let got = send(
            &gateway.router,
            Request::post("/v1/modrinth/version_files").header("content-type", "application/json"),
            &format!(r#"{{"hashes":["{JEI}","{SODIUM}","{other}","{JEI}"],"algorithm":"sha1"}}"#),
        )
        .await;
        assert_eq!(got.status, StatusCode::OK);
        assert_eq!(got.header("x-cache"), Some("miss"));
        let body = got.json();
        assert_eq!(body.as_object().unwrap().len(), 2);
        assert_eq!(body[JEI]["files"][0]["hashes"]["sha1"], JEI);
        assert_eq!(body[other]["files"][0]["hashes"]["sha1"], other);
        let seen = mock.seen();
        assert_eq!(seen.len(), 2);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&seen[1].body).unwrap()["hashes"],
            serde_json::json!([SODIUM, other])
        );

        let again = send(
            &gateway.router,
            Request::post("/v1/modrinth/version_files").header("content-type", "application/json"),
            &format!(r#"{{"hashes":["{SODIUM}","{other}"]}}"#),
        )
        .await;
        assert_eq!(again.header("x-cache"), Some("hit"));
        assert_eq!(mock.calls(), 2);
    }

    #[tokio::test]
    async fn refuses_a_bad_hash_body_or_algorithm() {
        let mock = Mock::start(Duration::ZERO, |_, _| reply(200, "{}")).await;
        let gateway = gateway(&mock, 300, TIMEOUT);

        let got = get(&gateway.router, "/v1/modrinth/version_file/not-a-hash").await;
        assert_eq!(got.status, StatusCode::BAD_REQUEST);
        assert_eq!(got.json()["code"], "INVALID_HASH");

        let many: Vec<String> = (0..1001).map(|n| format!("\"{n:040x}\"")).collect();
        let many = format!(r#"{{"hashes":[{}]}}"#, many.join(","));
        for (body, code) in [
            ("{}", "INVALID_BODY"),
            ("nope", "INVALID_BODY"),
            (r#"{"hashes":[]}"#, "INVALID_BODY"),
            (many.as_str(), "INVALID_BODY"),
            (r#"{"hashes":["abc"]}"#, "INVALID_HASH"),
        ] {
            let got = send(
                &gateway.router,
                Request::post("/v1/modrinth/version_files")
                    .header("content-type", "application/json"),
                body,
            )
            .await;
            assert_eq!(got.status, StatusCode::BAD_REQUEST, "{body}");
            assert_eq!(got.json()["code"], code, "{body}");
        }
        assert_eq!(mock.calls(), 0);
    }

    #[tokio::test]
    async fn a_failure_passes_through_and_a_kept_answer_is_given_stale() {
        let mock = Mock::start(Duration::ZERO, |call, _| {
            if call == 0 {
                reply(200, format!(r#"{{"{JEI}":{}}}"#, version(JEI, "jei.jar")))
            } else {
                reply(503, r#"{"error":"down"}"#)
            }
        })
        .await;
        let gateway = gateway(&mock, 300, TIMEOUT);
        get(&gateway.router, &format!("/v1/modrinth/version_file/{JEI}")).await;

        gateway.clock.advance(FOUND_MS + 1);
        let stale = get(&gateway.router, &format!("/v1/modrinth/version_file/{JEI}")).await;
        assert_eq!(stale.status, StatusCode::OK);
        assert_eq!(stale.header("x-cache"), Some("stale"));

        let fresh = get(
            &gateway.router,
            &format!("/v1/modrinth/version_file/{SODIUM}"),
        )
        .await;
        assert_eq!(fresh.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(fresh.header("x-cache"), Some("miss"));
        assert_eq!(fresh.json()["error"], "down");
    }

    #[tokio::test]
    async fn a_search_passes_through_with_the_query_and_stays_fresh_five_minutes() {
        let mock = Mock::start(Duration::ZERO, |_, _| {
            reply(
                200,
                r#"{"hits":[{"project_id":"AANobbMI","slug":"sodium"}],"total_hits":1}"#,
            )
        })
        .await;
        let gateway = gateway(&mock, 300, TIMEOUT);

        let got = get(
            &gateway.router,
            "/v1/modrinth/search?query=sodium&facets=%5B%5B%22project_type%3Amod%22%5D%5D",
        )
        .await;
        assert_eq!(got.status, StatusCode::OK);
        assert_eq!(got.header("x-cache"), Some("miss"));
        assert_eq!(got.json()["hits"][0]["slug"], "sodium");
        let seen = mock.seen();
        assert_eq!(seen[0].method, "GET");
        assert_eq!(
            seen[0].uri,
            "/v2/search?query=sodium&facets=%5B%5B%22project_type%3Amod%22%5D%5D"
        );
        assert_eq!(seen[0].key, None);

        let again = get(
            &gateway.router,
            "/v1/modrinth/search?facets=%5B%5B%22project_type%3Amod%22%5D%5D&query=sodium",
        )
        .await;
        assert_eq!(again.header("x-cache"), Some("hit"));
        gateway.clock.advance(5 * MINUTE_MS + 1);
        get(&gateway.router, "/v1/modrinth/search?query=sodium").await;
        assert_eq!(mock.calls(), 2);
        let modrinth = gateway.state.modrinth_budget.snapshot().await;
        assert_eq!(modrinth.days[0].pools[0].counts.calls, 2);
        assert_eq!(modrinth.days[0].pools[0].counts.hits, 1);
        assert_eq!(gateway.state.budget.snapshot().await.days, []);
    }

    #[tokio::test]
    async fn a_429_with_a_ratelimit_reset_holds_modrinth_off_but_not_curseforge() {
        let mock = Mock::start(Duration::ZERO, |call, seen| {
            if seen.uri.starts_with("/v2/") && call == 0 {
                Reply {
                    status: 429,
                    headers: vec![("x-ratelimit-reset", "7")],
                    body: String::new(),
                }
            } else {
                reply(200, r#"{"data":[],"hits":[]}"#)
            }
        })
        .await;
        let gateway = gateway(&mock, 300, TIMEOUT);

        let got = get(&gateway.router, "/v1/modrinth/project/sodium").await;
        assert_eq!(got.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(got.json()["code"], "UPSTREAM_HOLD");
        assert_eq!(got.json()["retryAfterMs"], 7_000);

        let held = get(&gateway.router, "/v1/modrinth/search?query=x").await;
        assert_eq!(held.json()["code"], "UPSTREAM_HOLD");
        let curseforge = get(&gateway.router, "/v1/curseforge/mods/search?gameId=432").await;
        assert_eq!(curseforge.status, StatusCode::OK);
        assert_eq!(mock.calls(), 2);

        gateway.clock.advance(7_001);
        let after = get(&gateway.router, "/v1/modrinth/search?query=x").await;
        assert_eq!(after.status, StatusCode::OK);
        assert!(
            gateway
                .state
                .modrinth_budget
                .snapshot()
                .await
                .held_until
                .is_none()
        );
    }

    #[tokio::test]
    async fn another_hash_algorithm_goes_to_modrinth_as_it_came() {
        let mock = Mock::start(Duration::ZERO, |_, _| reply(200, r#"{"x":{}}"#)).await;
        let gateway = gateway(&mock, 300, TIMEOUT);
        for _ in 0..2 {
            let got = send(
                &gateway.router,
                Request::post("/v1/modrinth/version_files")
                    .header("content-type", "application/json"),
                r#"{"hashes":["abc"],"algorithm":"sha512"}"#,
            )
            .await;
            assert_eq!(got.status, StatusCode::OK);
            assert_eq!(got.header("x-cache"), Some("miss"));
            assert_eq!(got.json()["x"], serde_json::json!({}));
        }
        let seen = mock.seen();
        assert_eq!(seen.len(), 2);
        assert!(seen[0].body.contains("sha512"));
        let single = get(
            &gateway.router,
            "/v1/modrinth/version_file/abc?algorithm=sha512",
        )
        .await;
        assert_eq!(single.status, StatusCode::OK);
        assert_eq!(mock.seen()[2].uri, "/v2/version_file/abc?algorithm=sha512");
    }

    #[tokio::test]
    async fn a_slug_with_the_characters_modrinth_allows_passes_and_a_dot_segment_does_not() {
        let mock = Mock::start(Duration::ZERO, |_, _| reply(200, r#"{"slug":"x"}"#)).await;
        let gateway = gateway(&mock, 300, TIMEOUT);
        let got = get(
            &gateway.router,
            "/v1/modrinth/project/fabric-api.v1+jar(a)!",
        )
        .await;
        assert_eq!(got.status, StatusCode::OK);
        assert_eq!(mock.seen()[0].uri, "/v2/project/fabric-api.v1+jar(a)!");
        for uri in [
            "/v1/modrinth/project/../search",
            "/v1/modrinth/project/.",
            "/v1/modrinth/project/a%25b",
            "/v1/curseforge/mods/fabric-api.v1",
        ] {
            let got = get(&gateway.router, uri).await;
            assert_eq!(got.status, StatusCode::BAD_REQUEST, "{uri}");
            assert_eq!(got.json()["code"], "INVALID_PATH", "{uri}");
        }
        assert_eq!(mock.calls(), 1);
    }

    #[tokio::test]
    async fn a_post_to_another_route_goes_to_modrinth_uncached() {
        let mock = Mock::start(Duration::ZERO, |_, _| reply(200, "{}")).await;
        let gateway = gateway(&mock, 300, TIMEOUT);
        for _ in 0..2 {
            let got = send(
                &gateway.router,
                Request::post("/v1/modrinth/version_files/update")
                    .header("content-type", "application/json"),
                r#"{"hashes":["a"],"algorithm":"sha1","loaders":["fabric"],"game_versions":["1.21"]}"#,
            )
            .await;
            assert_eq!(got.status, StatusCode::OK);
            assert_eq!(got.header("x-cache"), Some("miss"));
        }
        let seen = mock.seen();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].uri, "/v2/version_files/update");
        assert!(seen[0].body.contains("fabric"));
    }

    #[test]
    fn the_cache_table_covers_what_a_launcher_reads() {
        let found =
            |path: &str| freshness(&path.split('/').collect::<Vec<_>>()).map(|f| f.found_ms);
        assert_eq!(found("search"), Some(5 * MINUTE_MS));
        assert_eq!(found("tag/category"), Some(DAY_MS));
        assert_eq!(found("project/sodium"), Some(HOUR_MS));
        assert_eq!(found("project/AANobbMI/version"), Some(10 * MINUTE_MS));
        assert_eq!(found("version/uMOpc5uV"), Some(10 * MINUTE_MS));
        assert_eq!(found("versions"), Some(10 * MINUTE_MS));
        assert_eq!(found("user/mezz"), Some(HOUR_MS));
        assert_eq!(found("version_file/abc"), None);
        assert_eq!(found("statistics"), None);
    }
}
