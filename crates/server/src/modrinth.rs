use std::{collections::HashMap, sync::Arc, time::Duration};

use axum::{
    Json,
    body::Bytes,
    extract::{Path, State, rejection::JsonRejection},
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{ACCEPT, CONTENT_TYPE},
    },
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::value::RawValue;
use utoipa::ToSchema;

use crate::{
    app::AppState,
    batch,
    cache::{Answer, AnswerCache, Entry, Finisher, LoadFailure, Outcome, Waiting},
    curseforge::{self, HOUR_MS, MINUTE_MS, UpstreamFailure},
    error::ApiError,
};

pub const MODRINTH_API: &str = "https://api.modrinth.com";
pub const TIMEOUT: Duration = Duration::from_secs(15);
/// Modrinth asks every client to say who it is; this is what the gateway says unless `MODRINTH_USER_AGENT` is set.
pub const DEFAULT_USER_AGENT: &str = concat!("content-gateway/", env!("CARGO_PKG_VERSION"));
pub const SOURCE: &str = "Modrinth";
const HASHES_PER_CALL: usize = 100;
const FOUND_MS: u64 = HOUR_MS;
const NOT_FOUND_MS: u64 = 10 * MINUTE_MS;
const SHA1_LENGTH: usize = 40;

/// The client that asks Modrinth, with the User-Agent Modrinth asks for.
pub struct Modrinth {
    client: reqwest::Client,
    base: String,
}

impl Modrinth {
    pub fn new(
        base: impl Into<String>,
        user_agent: &str,
        timeout: Duration,
    ) -> reqwest::Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(timeout)
                .user_agent(user_agent)
                .build()?,
            base: base.into(),
        })
    }

    async fn version_files(&self, hashes: &[String]) -> Result<Answer, UpstreamFailure> {
        let body = serde_json::json!({ "hashes": hashes, "algorithm": "sha1" });
        let response = self
            .client
            .post(format!("{}/v2/version_files", self.base))
            .header(ACCEPT, "application/json")
            .header(CONTENT_TYPE, "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|error| curseforge::failure(error, SOURCE))?;
        curseforge::read(response, SOURCE)
            .await
            .map(|(answer, _)| answer)
    }
}

/// The body of `POST /v1/modrinth/version_files`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct VersionFilesRequest {
    /// SHA-1 hashes of the wanted files, 40 hex digits each.
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
/// call per 100, joined with any call for the same hash already in flight. A hash Modrinth does not know is kept as
/// such for 10 minutes, a found version for an hour. Kept answers past their fresh time are given when Modrinth fails.
pub async fn lookup(
    state: &Arc<AppState>,
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
        tokio::spawn(fetch(state.clone(), mine, forced));
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

async fn fetch(state: Arc<AppState>, mine: Vec<Mine>, forced: bool) {
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
    let mut missing = missing.into_iter().peekable();
    while missing.peek().is_some() {
        let chunk: Vec<Mine> = missing.by_ref().take(HASHES_PER_CALL).collect();
        tokio::spawn(fetch_chunk(state.clone(), chunk));
    }
}

async fn fetch_chunk(state: Arc<AppState>, chunk: Vec<Mine>) {
    let hashes: Vec<String> = chunk.iter().map(|load| load.hash.clone()).collect();
    let found = match state.modrinth.version_files(&hashes).await {
        Ok(answer) if answer.status == StatusCode::OK => match split(&answer) {
            Some(found) => Ok(found),
            None => Err(LoadFailure::Upstream(UpstreamFailure::Unreadable)),
        },
        Ok(answer) => Err(LoadFailure::Answered(answer)),
        Err(failure) => Err(LoadFailure::Upstream(failure)),
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

fn failed(failure: LoadFailure) -> Response {
    match failure {
        LoadFailure::Answered(answer) => curseforge::respond(&answer, "miss"),
        LoadFailure::Refused(_) => {
            ApiError::upstream(UpstreamFailure::Unreachable, SOURCE).into_response()
        }
        LoadFailure::Upstream(failure) => ApiError::upstream(failure, SOURCE).into_response(),
    }
}

/// The Modrinth version of the file with this SHA-1, as `GET api.modrinth.com/v2/version_file/{hash}` answers it.
///
/// Kept for an hour once found and for 10 minutes when Modrinth does not know the hash. `Cache-Control: no-cache`
/// asks Modrinth again. Modrinth calls do not count against the CurseForge budget.
#[utoipa::path(
    get,
    path = "/v1/modrinth/version_file/{hash}",
    tag = "modrinth",
    security(("bearer" = [])),
    params(
        ("hash" = String, Path, description = "The file's SHA-1, 40 hex digits", example = "3e1dd9fb5ea2f0dd4ffcc2b1a7d0bb1a1a9d6a8e"),
        ("Cache-Control" = Option<String>, Header, description = "`no-cache` asks Modrinth again instead of answering from the cache"),
    ),
    responses(
        (status = 200, description = "The version Modrinth lists the file under, as Modrinth sent it", body = Value, headers(("X-Cache" = String, description = "`hit` from the cache, `miss` from Modrinth, `stale` an old answer given because Modrinth could not be asked"))),
        (status = 400, description = "`INVALID_HASH`", body = ApiError),
        (status = 401, description = "No valid consumer token", body = ApiError),
        (status = 404, description = "Modrinth does not know the hash", headers(("X-Cache" = String, description = "As on 200"))),
        (status = 502, description = "Modrinth could not be reached or answered something the gateway cannot keep: `UPSTREAM_UNREACHABLE`, `UPSTREAM_TOO_LARGE` or `UPSTREAM_UNREADABLE`", body = ApiError),
        (status = 504, description = "Modrinth did not answer in time, `UPSTREAM_TIMEOUT`", body = ApiError),
        (status = "default", description = "Any other status Modrinth answered, with its body. Only the gateway's own answers lack `X-Cache`"),
    )
)]
pub async fn version_file(
    State(state): State<Arc<AppState>>,
    Path(hash): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Some(hash) = sha1_of(&hash) else {
        return ApiError::invalid_hash().into_response();
    };
    let looked = match lookup(&state, vec![hash], curseforge::wants_fresh(&headers)).await {
        Ok(looked) => looked,
        Err(failure) => return failed(failure),
    };
    match looked
        .known
        .into_iter()
        .next()
        .and_then(|known| known.version)
    {
        Some(version) => curseforge::respond(
            &Answer {
                status: StatusCode::OK,
                content_type: Some(json()),
                body: version,
            },
            looked.cache,
        ),
        None => curseforge::respond(
            &Answer {
                status: StatusCode::NOT_FOUND,
                content_type: None,
                body: Bytes::new(),
            },
            looked.cache,
        ),
    }
}

/// The Modrinth versions of the files with these SHA-1s, keyed by hash, as `POST api.modrinth.com/v2/version_files`
/// answers them: a hash Modrinth does not know is left out.
///
/// Answered per hash from the cache; only the hashes not kept are asked for, 100 per call.
#[utoipa::path(
    post,
    path = "/v1/modrinth/version_files",
    tag = "modrinth",
    security(("bearer" = [])),
    params(
        ("Cache-Control" = Option<String>, Header, description = "`no-cache` asks Modrinth again instead of answering from the cache"),
    ),
    request_body(content = VersionFilesRequest, content_type = "application/json"),
    responses(
        (status = 200, description = "The versions found, keyed by lowercase hash, each as Modrinth sent it", body = Value, headers(("X-Cache" = String, description = "`hit` when every hash was kept, `miss` when Modrinth was asked, `stale` an old answer given because Modrinth could not be asked"))),
        (status = 400, description = "`INVALID_BODY` or `INVALID_HASH`", body = ApiError),
        (status = 401, description = "No valid consumer token", body = ApiError),
        (status = 502, description = "Modrinth could not be reached or answered something the gateway cannot keep: `UPSTREAM_UNREACHABLE`, `UPSTREAM_TOO_LARGE` or `UPSTREAM_UNREADABLE`", body = ApiError),
        (status = 504, description = "Modrinth did not answer in time, `UPSTREAM_TIMEOUT`", body = ApiError),
        (status = "default", description = "Any other status Modrinth answered, with its body. Only the gateway's own answers lack `X-Cache`"),
    )
)]
pub async fn version_files(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Result<Json<VersionFilesRequest>, JsonRejection>,
) -> Response {
    let Ok(Json(request)) = body else {
        return ApiError::invalid_body("a JSON object with `hashes`, a list of SHA-1 hashes")
            .into_response();
    };
    if !request.algorithm.eq_ignore_ascii_case("sha1") {
        return ApiError::invalid_body("only the `sha1` algorithm is supported").into_response();
    }
    if request.hashes.is_empty() {
        return ApiError::invalid_body("`hashes` must list at least one SHA-1").into_response();
    }
    let mut hashes = Vec::with_capacity(request.hashes.len());
    for hash in &request.hashes {
        let Some(hash) = sha1_of(hash) else {
            return ApiError::invalid_hash().into_response();
        };
        if !hashes.contains(&hash) {
            hashes.push(hash);
        }
    }
    let looked = match lookup(&state, hashes, curseforge::wants_fresh(&headers)).await {
        Ok(looked) => looked,
        Err(failure) => return failed(failure),
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
    curseforge::respond(
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

        let again = get(
            &gateway.router,
            &format!("/v1/modrinth/version_file/{}", JEI.to_ascii_uppercase()),
        )
        .await;
        assert_eq!(again.header("x-cache"), Some("hit"));
        assert_eq!(mock.calls(), 1);
        assert_eq!(gateway.state.budget.snapshot().await.used, 0);
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

        for (body, code) in [
            ("{}", "INVALID_BODY"),
            (r#"{"hashes":[]}"#, "INVALID_BODY"),
            (r#"{"hashes":["abc"],"algorithm":"sha512"}"#, "INVALID_BODY"),
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
    async fn a_version_file_route_needs_no_pool_header() {
        let mock = Mock::start(Duration::ZERO, |_, _| reply(200, "{}")).await;
        let gateway = gateway_with_pools(&mock, &["launcher", "app"]);
        let got = get(&gateway.router, &format!("/v1/modrinth/version_file/{JEI}")).await;
        assert_eq!(got.status, StatusCode::NOT_FOUND);
    }
}
