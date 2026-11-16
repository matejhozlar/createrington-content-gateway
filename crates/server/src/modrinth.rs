use std::{collections::HashMap, sync::Arc, time::Duration};

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
        (status = 500, description = "The gateway failed to finish the request: `INTERNAL`", body = ApiError),
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
        (status = 500, description = "The gateway failed to finish the request: `INTERNAL`", body = ApiError),
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
    let Some(hashes) = request
        .hashes
        .iter()
        .map(|hash| sha1_of(hash))
        .collect::<Option<Vec<String>>>()
    else {
        return ApiError::invalid_hash().into_response();
    };
    let Some(hashes) = load::distinct(hashes, MOST_HASHES) else {
        return ApiError::invalid_body(&format!("`hashes` may list at most {MOST_HASHES} SHA-1s"))
            .into_response();
    };
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
mod tests;
