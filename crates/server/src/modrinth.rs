use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, Method, Uri},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::value::RawValue;
use utoipa::ToSchema;

use crate::{
    app::AppState,
    auth::Pool,
    cache::{Answer, Entry},
    clock::{DAY_MS, HOUR_MS, MINUTE_MS},
    error::ApiError,
    load::{self, PerId},
    passthrough::{self, Freshness, SHA1_LENGTH, fresh, sha1_of},
    upstream::Source,
};

pub const MODRINTH_API: &str = "https://api.modrinth.com/v2";
pub const TIMEOUT: Duration = Duration::from_secs(10);
const MOST_HASHES: usize = 1000;
const FOUND_MS: u64 = HOUR_MS;
const NOT_FOUND_MS: u64 = 10 * MINUTE_MS;

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

/// Whether a `version_files` body asks for SHA-1 (or nothing), the lookup kept per hash.
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
    /// `sha1`, kept per hash. Any other algorithm sends the body to Modrinth as it came, uncached.
    #[serde(default = "sha1")]
    #[schema(example = "sha1")]
    #[expect(
        dead_code,
        reason = "documents the body; `sha1_body` reads the algorithm"
    )]
    pub algorithm: String,
}

fn sha1() -> String {
    "sha1".to_owned()
}

/// Modrinth's version of the file with each lowercase SHA-1, kept per hash and asked for with `POST version_files`.
#[derive(Debug, Clone, Copy)]
pub struct VersionFiles;

impl PerId for VersionFiles {
    type Id = String;
    const SOURCE: Source = Source::Modrinth;
    const PER_CALL: usize = 100;
    const KNOWS_NONE_AS_404: bool = false;

    fn key(self, hash: &String) -> String {
        format!("modrinth:sha1:{hash}")
    }

    fn path(self) -> &'static str {
        "version_files"
    }

    fn body(self, hashes: &[String]) -> Bytes {
        serde_json::json!({ "hashes": hashes, "algorithm": "sha1" })
            .to_string()
            .into()
    }

    fn split(self, answer: &Answer, now_ms: u64) -> Option<Vec<(String, Entry)>> {
        let versions: HashMap<String, &RawValue> = serde_json::from_slice(&answer.body).ok()?;
        Some(
            versions
                .into_iter()
                .map(|(hash, version)| {
                    let version = answer.body.slice_ref(version.get().as_bytes());
                    let mut entry = Entry::new(Answer::json(version.clone()), now_ms, FOUND_MS);
                    entry.row = Some(version);
                    (hash.to_ascii_lowercase(), entry)
                })
                .collect(),
        )
    }

    fn not_found_ms(self) -> u64 {
        NOT_FOUND_MS
    }
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
        ("path" = String, Path, description = "The path under `api.modrinth.com/v2/`, such as `search`, `project/sodium` or `version_file/{sha1}`: segments of letters, digits, `-`, `_` and any of `.+!()~',@$`", example = "search"),
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
        ("path" = String, Path, description = "The path under `api.modrinth.com/v2/`, such as `version_files` or `version_files/update`: segments of letters, digits, `-`, `_` and any of `.+!()~',@$`", example = "version_files"),
        ("X-Budget-Pool" = Option<String>, Header, description = "The pool the call draws from. Required when the token has more than one pool"),
        ("Cache-Control" = Option<String>, Header, description = "`no-cache` asks Modrinth again instead of answering from the cache"),
    ),
    request_body(content = VersionFilesRequest, description = "The JSON body Modrinth expects at this path; this shape for `version_files`", content_type = "application/json"),
    responses(
        (status = 200, description = "Modrinth's answer, as it came, or the versions found keyed by hash for `version_files`", body = Value, headers(("X-Cache" = String, description = "`hit` when every hash was kept, `miss` when Modrinth was asked, `stale` an old answer given because Modrinth could not be asked"))),
        (status = 400, description = "`INVALID_PATH`, `INVALID_BODY` (no hashes or more than 1000), `INVALID_HASH`, `POOL_REQUIRED` or `UNKNOWN_POOL`", body = ApiError),
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
    let given = match load::rows(&state, &pool, VersionFiles, vec![hash], forced).await {
        Ok(given) => given,
        Err(failure) => return passthrough::failed(failure, &pool, Source::Modrinth),
    };
    match given.value.into_iter().next() {
        Some((_, version)) => passthrough::respond(&Answer::json(version), given.cache),
        None => passthrough::respond(&Answer::not_found(), given.cache),
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
    let given = match load::rows(&state, &pool, VersionFiles, hashes, forced).await {
        Ok(given) => given,
        Err(failure) => return passthrough::failed(failure, &pool, Source::Modrinth),
    };
    let found = given.value;
    let mut body = Vec::with_capacity(
        found
            .iter()
            .map(|(_, version)| version.len() + SHA1_LENGTH + 4)
            .sum::<usize>()
            + 2,
    );
    body.push(b'{');
    for (index, (hash, version)) in found.iter().enumerate() {
        if index > 0 {
            body.push(b',');
        }
        body.push(b'"');
        body.extend_from_slice(hash.as_bytes());
        body.extend_from_slice(b"\":");
        body.extend_from_slice(version);
    }
    body.push(b'}');
    passthrough::respond(&Answer::json(body), given.cache)
}

#[cfg(test)]
mod tests {
    use axum::http::{Request, StatusCode};

    use super::*;
    use crate::{app::Timeouts, test_support::*};

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
        let gateway = gateway(&mock);

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
        assert_eq!(gateway.state.curseforge_budget.snapshot().await.used, 0);
        let modrinth = gateway.state.modrinth_budget.snapshot().await;
        assert_eq!(modrinth.used, 1);
        assert_eq!(modrinth.days[0].pools[0].counts.calls, 1);
        assert_eq!(modrinth.days[0].pools[0].counts.hits, 1);
    }

    #[tokio::test]
    async fn a_hash_modrinth_does_not_know_answers_404_and_is_kept_ten_minutes() {
        let mock = Mock::start(Duration::ZERO, |_, _| reply(200, "{}")).await;
        let gateway = gateway(&mock);

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
        let gateway = gateway(&mock);
        get(&gateway.router, &format!("/v1/modrinth/version_file/{JEI}")).await;

        let other = "abcdefabcdefabcdefabcdefabcdefabcdefabcd";
        let got = post_json(
            &gateway.router,
            "/v1/modrinth/version_files",
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

        let again = post_json(
            &gateway.router,
            "/v1/modrinth/version_files",
            &format!(r#"{{"hashes":["{SODIUM}","{other}"]}}"#),
        )
        .await;
        assert_eq!(again.header("x-cache"), Some("hit"));
        assert_eq!(mock.calls(), 2);
    }

    #[tokio::test]
    async fn refuses_a_bad_hash_or_body() {
        let mock = Mock::start(Duration::ZERO, |_, _| reply(200, "{}")).await;
        let gateway = gateway(&mock);

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
            let got = post_json(&gateway.router, "/v1/modrinth/version_files", body).await;
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
        let gateway = gateway(&mock);
        get(&gateway.router, &format!("/v1/modrinth/version_file/{JEI}")).await;

        gateway.clock.advance(FOUND_MS + 1);
        let stale = get(&gateway.router, &format!("/v1/modrinth/version_file/{JEI}")).await;
        assert_eq!(stale.status, StatusCode::OK);
        assert_eq!(stale.header("x-cache"), Some("stale"));
        let snapshot = gateway.state.modrinth_budget.snapshot().await;
        let counts = &snapshot.days[0].pools[0].counts;
        assert_eq!((counts.calls, counts.hits), (2, 0));

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
    async fn a_modrinth_that_does_not_answer_in_time_is_a_504() {
        let (mock, _never) = Mock::held(|_, _| reply(200, "{}")).await;
        let gateway = gateway_with(
            &mock,
            GatewayWith {
                timeouts: Timeouts {
                    modrinth: Duration::from_millis(50),
                    ..Timeouts::default()
                },
                ..GatewayWith::default()
            },
        );
        let got = get(&gateway.router, "/v1/modrinth/project/jei").await;
        assert_eq!(got.status, StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(got.json()["code"], "UPSTREAM_TIMEOUT");
        assert_eq!(
            gateway.state.modrinth_budget.snapshot().await.days[0].pools[0]
                .counts
                .calls,
            1
        );
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
        let gateway = gateway(&mock);

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
        assert_eq!(gateway.state.curseforge_budget.snapshot().await.days, []);
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
        let gateway = gateway(&mock);

        let got = get(&gateway.router, "/v1/modrinth/project/sodium").await;
        assert_eq!(got.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(got.json()["code"], "UPSTREAM_HOLD");
        assert_eq!(
            got.json()["message"],
            "Modrinth answered 429, every call waits until the hold ends"
        );
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
        let gateway = gateway(&mock);
        for _ in 0..2 {
            let got = post_json(
                &gateway.router,
                "/v1/modrinth/version_files",
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
        let gateway = gateway(&mock);
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
            let named = if uri.starts_with("/v1/modrinth/") {
                "a Modrinth path"
            } else {
                "a CurseForge path"
            };
            assert!(
                got.json()["message"].as_str().unwrap().starts_with(named),
                "{uri}"
            );
        }
        assert_eq!(mock.calls(), 1);
    }

    #[tokio::test]
    async fn a_post_to_another_route_goes_to_modrinth_uncached() {
        let mock = Mock::start(Duration::ZERO, |_, _| reply(200, "{}")).await;
        let gateway = gateway(&mock);
        for _ in 0..2 {
            let got = post_json(
                &gateway.router,
                "/v1/modrinth/version_files/update",
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

    fn every_hash(_: usize, seen: &Seen) -> Reply {
        let versions: Vec<String> = asked_in(seen)
            .iter()
            .map(|hash| format!(r#""{hash}":{}"#, version(hash, "mod.jar")))
            .collect();
        reply(200, format!("{{{}}}", versions.join(",")))
    }

    fn hashes(count: u64) -> Vec<String> {
        (1..=count).map(|n| format!("{n:040x}")).collect()
    }

    fn hashes_body(hashes: &[String]) -> String {
        serde_json::json!({ "hashes": hashes }).to_string()
    }

    fn asked_in(seen: &Seen) -> Vec<String> {
        let asked: serde_json::Value = serde_json::from_str(&seen.body).unwrap();
        asked["hashes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|hash| hash.as_str().unwrap().to_owned())
            .collect()
    }

    #[tokio::test]
    async fn asks_for_a_hundred_hashes_a_call_and_counts_each_call() {
        let mock = Mock::start(Duration::ZERO, every_hash).await;
        let gateway = gateway(&mock);
        let wanted = hashes(250);

        let got = post_json(
            &gateway.router,
            "/v1/modrinth/version_files",
            &hashes_body(&wanted),
        )
        .await;
        assert_eq!(got.status, StatusCode::OK);
        assert_eq!(got.json().as_object().unwrap().len(), 250);

        let mut sizes: Vec<usize> = mock
            .seen()
            .iter()
            .map(|seen| asked_in(seen).len())
            .collect();
        sizes.sort_unstable();
        assert_eq!(sizes, [50, 100, 100]);
        let mut asked: Vec<String> = mock.seen().iter().flat_map(asked_in).collect();
        asked.sort();
        assert_eq!(asked, wanted);
        assert_eq!(gateway.state.modrinth_budget.snapshot().await.used, 3);
    }

    #[tokio::test]
    async fn hashes_needing_more_calls_than_a_minute_allows_are_refused_whole() {
        let mock = Mock::start(Duration::ZERO, every_hash).await;
        let gateway = gateway_with(
            &mock,
            GatewayWith {
                limit: 2,
                ..GatewayWith::default()
            },
        );
        let got = post_json(
            &gateway.router,
            "/v1/modrinth/version_files",
            &hashes_body(&hashes(201)),
        )
        .await;
        assert_eq!(got.status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(got.json()["code"], "REQUEST_TOO_LARGE");
        assert_eq!(mock.calls(), 0);
    }

    #[tokio::test]
    async fn a_spent_budget_refuses_a_hash_lookup_and_keeps_nothing() {
        let mock = Mock::start(Duration::ZERO, every_hash).await;
        let gateway = gateway_with(
            &mock,
            GatewayWith {
                limit: 1,
                ..GatewayWith::default()
            },
        );
        get(&gateway.router, &format!("/v1/modrinth/version_file/{JEI}")).await;

        let refused = get(
            &gateway.router,
            &format!("/v1/modrinth/version_file/{SODIUM}"),
        )
        .await;
        assert_eq!(refused.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(refused.json()["code"], "BUDGET_SPENT");
        assert!(refused.header("retry-after").is_some());
        assert_eq!(mock.calls(), 1);

        gateway.clock.advance(MINUTE_MS);
        let later = get(
            &gateway.router,
            &format!("/v1/modrinth/version_file/{SODIUM}"),
        )
        .await;
        assert_eq!(later.header("x-cache"), Some("miss"));
        assert_eq!(mock.calls(), 2);
    }

    #[tokio::test]
    async fn version_files_joins_a_lookup_of_the_same_hash_already_running() {
        let (mock, gate) = Mock::held(every_hash).await;
        let gateway = gateway(&mock);
        let single = {
            let router = gateway.router.clone();
            tokio::spawn(
                async move { get(&router, &format!("/v1/modrinth/version_file/{JEI}")).await },
            )
        };
        until("the single lookup to reach Modrinth", || mock.calls() == 1).await;
        let joined = {
            let router = gateway.router.clone();
            tokio::spawn(async move {
                post_json(
                    &router,
                    "/v1/modrinth/version_files",
                    &format!(r#"{{"hashes":["{JEI}"]}}"#),
                )
                .await
            })
        };
        until("version_files joining it", || {
            gateway.state.cache.joined() == 1
        })
        .await;
        gate.open();

        let got = joined.await.unwrap();
        assert_eq!(got.header("x-cache"), Some("hit"));
        assert_eq!(got.json()[JEI]["files"][0]["filename"], "mod.jar");
        assert_eq!(single.await.unwrap().header("x-cache"), Some("miss"));
        assert_eq!(mock.calls(), 1);
    }

    #[tokio::test]
    async fn an_answer_that_is_not_versions_by_hash_is_unreadable_and_not_kept() {
        let mock = Mock::start(Duration::ZERO, |_, _| reply(200, "[]")).await;
        let gateway = gateway(&mock);
        for _ in 0..2 {
            let got = get(&gateway.router, &format!("/v1/modrinth/version_file/{JEI}")).await;
            assert_eq!(got.status, StatusCode::BAD_GATEWAY);
            assert_eq!(got.json()["code"], "UPSTREAM_UNREADABLE");
        }
        assert_eq!(mock.calls(), 2);
    }

    #[tokio::test]
    async fn no_cache_asks_modrinth_again_for_kept_hashes() {
        let mock = Mock::start(Duration::ZERO, every_hash).await;
        let gateway = gateway(&mock);
        get(&gateway.router, &format!("/v1/modrinth/version_file/{JEI}")).await;

        let single = send(
            &gateway.router,
            Request::get(format!("/v1/modrinth/version_file/{JEI}"))
                .header("cache-control", "no-cache"),
            "",
        )
        .await;
        assert_eq!(single.header("x-cache"), Some("miss"));
        assert_eq!(mock.calls(), 2);

        let many = send(
            &gateway.router,
            Request::post("/v1/modrinth/version_files")
                .header("content-type", "application/json")
                .header("cache-control", "no-cache"),
            &format!(r#"{{"hashes":["{JEI}","{SODIUM}"]}}"#),
        )
        .await;
        assert_eq!(many.header("x-cache"), Some("miss"));
        assert_eq!(mock.calls(), 3);
        assert_eq!(asked_in(&mock.seen()[2]), [JEI, SODIUM]);
    }

    #[tokio::test]
    async fn a_hash_lookup_modrinth_does_not_answer_in_time_is_a_504() {
        let (mock, _never) = Mock::held(every_hash).await;
        let gateway = gateway_with(
            &mock,
            GatewayWith {
                timeouts: Timeouts {
                    modrinth: Duration::from_millis(50),
                    ..Timeouts::default()
                },
                ..GatewayWith::default()
            },
        );
        let got = get(&gateway.router, &format!("/v1/modrinth/version_file/{JEI}")).await;
        assert_eq!(got.status, StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(got.json()["code"], "UPSTREAM_TIMEOUT");
    }
}
