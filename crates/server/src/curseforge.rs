use std::{sync::Arc, time::Duration};

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, Method, Uri},
    response::Response,
};

use crate::{
    app::AppState,
    auth::Pool,
    batch::Batch,
    clock::{DAY_MS, HOUR_MS, MINUTE_MS},
    error::ApiError,
    load::PerId,
    passthrough::{self, Freshness, fresh, is_id},
    upstream::Source,
};

pub const CURSEFORGE_API: &str = "https://api.curseforge.com/v1";
pub const TIMEOUT: Duration = Duration::from_secs(10);

/// The cache table of the CurseForge routes.
pub fn freshness(segments: &[&str]) -> Option<Freshness> {
    match segments {
        ["mods", "search"] => fresh(5 * MINUTE_MS, None),
        ["categories"] | ["minecraft", "version"] | ["minecraft", "modloader"] => {
            fresh(DAY_MS, None)
        }
        ["mods", project] if is_id(project) => {
            fresh(Batch::Mods.found_ms(), Some(Batch::Mods.not_found_ms()))
        }
        ["mods", project, "files"] if is_id(project) => fresh(10 * MINUTE_MS, None),
        ["mods", project, "description"] if is_id(project) => fresh(DAY_MS, None),
        ["mods", project, "files", file] if is_id(project) && is_id(file) => {
            fresh(Batch::Files.found_ms(), Some(Batch::Files.not_found_ms()))
        }
        ["mods", project, "files", file, "download-url"] if is_id(project) && is_id(file) => {
            fresh(HOUR_MS, Some(10 * MINUTE_MS))
        }
        ["mods", project, "files", file, "changelog"] if is_id(project) && is_id(file) => {
            fresh(DAY_MS, None)
        }
        _ => None,
    }
}

/// A single-id route whose answer is one row of a batch, kept under the batch's key for the id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Share {
    Mod(u64),
    File { project: u64, file: u64 },
}

pub fn share_of(segments: &[&str], query: Option<&str>) -> Option<Share> {
    if query.is_some_and(|query| !query.is_empty()) {
        return None;
    }
    match segments {
        ["mods", project] => project.parse().ok().map(Share::Mod),
        ["mods", project, "files", file] => Some(Share::File {
            project: project.parse().ok()?,
            file: file.parse().ok()?,
        }),
        _ => None,
    }
}

/// What `GET api.curseforge.com/v1/{path}` answers, from the cache while it is fresh.
///
/// The query string goes to CurseForge as it came, and the answer comes back with CurseForge's status, content type
/// and body, marked `X-Cache`. The routes of the cache table are kept for their fresh time; every other route is
/// asked each time. `Cache-Control: no-cache` asks CurseForge again and keeps the new answer. A kept answer that is
/// no longer fresh is still given, marked `stale`, when the budget refuses the call or CurseForge fails.
#[utoipa::path(
    get,
    operation_id = "curseforge_get",
    path = "/v1/curseforge/{path}",
    tag = "curseforge",
    security(("bearer" = [])),
    params(
        ("path" = String, Path, description = "The path under `api.curseforge.com/v1/`, such as `mods/search` or `mods/238222/files`: segments of letters, digits, `-` and `_`", example = "mods/search"),
        ("X-Budget-Pool" = Option<String>, Header, description = "The pool the call draws from. Required when the token has more than one pool"),
        ("Cache-Control" = Option<String>, Header, description = "`no-cache` asks CurseForge again instead of answering from the cache"),
    ),
    responses(
        (status = 200, description = "CurseForge's answer, as it came", body = Value, headers(("X-Cache" = String, description = "`hit` from the cache, `miss` from CurseForge, `stale` an old answer given because CurseForge could not be asked"))),
        (status = 400, description = "`INVALID_PATH`, `POOL_REQUIRED` or `UNKNOWN_POOL`", body = ApiError),
        (status = 401, description = "No valid consumer token", body = ApiError),
        (status = 500, description = "The gateway failed to finish the request: `INTERNAL`", body = ApiError),
        (status = 502, description = "CurseForge could not be reached or answered something the gateway cannot keep: `UPSTREAM_UNREACHABLE`, `UPSTREAM_TOO_LARGE` or `UPSTREAM_UNREADABLE`", body = ApiError),
        (status = 503, description = "The call was refused: `BUDGET_SPENT` this minute or `UPSTREAM_HOLD` after a 429 from CurseForge", body = ApiError, headers(("Retry-After" = u64, description = "Seconds until the next call can go out"))),
        (status = 504, description = "CurseForge did not answer in time, `UPSTREAM_TIMEOUT`", body = ApiError),
        (status = "default", description = "Any other status CurseForge answered, with its body. Only the gateway's own answers lack `X-Cache`"),
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
        Source::CurseForge,
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

/// What `POST api.curseforge.com/v1/{path}` answers to this body.
///
/// `POST mods` with only `{"modIds": [...]}`, `POST mods/files` with only `{"fileIds": [...]}`, and
/// `POST fingerprints/432` or `POST fingerprints` with only `{"fingerprints": [...]}` are answered per id from the
/// cache, and only the ids not kept are asked for, 100 per call, at most 1000 different ids. Every other body goes to
/// CurseForge each time.
#[utoipa::path(
    post,
    operation_id = "curseforge_post",
    path = "/v1/curseforge/{path}",
    tag = "curseforge",
    security(("bearer" = [])),
    params(
        ("path" = String, Path, description = "The path under `api.curseforge.com/v1/`, such as `mods` or `fingerprints/432`: segments of letters, digits, `-` and `_`", example = "mods"),
        ("X-Budget-Pool" = Option<String>, Header, description = "The pool the call draws from. Required when the token has more than one pool"),
        ("Cache-Control" = Option<String>, Header, description = "`no-cache` asks CurseForge again instead of answering from the cache"),
    ),
    request_body(content = Value, description = "The JSON body CurseForge expects at this path", content_type = "application/json"),
    responses(
        (status = 200, description = "CurseForge's answer, as it came, or one built from the kept rows of a batch", body = Value, headers(("X-Cache" = String, description = "`hit` when every id was kept, `miss` when CurseForge was asked, `stale` an old answer given because CurseForge could not be asked"))),
        (status = 400, description = "`INVALID_PATH`, `INVALID_BODY` (a batch of more than 1000 different ids), `POOL_REQUIRED` or `UNKNOWN_POOL`", body = ApiError),
        (status = 401, description = "No valid consumer token", body = ApiError),
        (status = 413, description = "The batch needs more calls than a whole minute allows, `REQUEST_TOO_LARGE`; split it", body = ApiError),
        (status = 500, description = "The gateway failed to finish the request: `INTERNAL`", body = ApiError),
        (status = 502, description = "CurseForge could not be reached or answered something the gateway cannot keep: `UPSTREAM_UNREACHABLE`, `UPSTREAM_TOO_LARGE` or `UPSTREAM_UNREADABLE`", body = ApiError),
        (status = 503, description = "The call was refused: `BUDGET_SPENT` this minute or `UPSTREAM_HOLD` after a 429 from CurseForge", body = ApiError, headers(("Retry-After" = u64, description = "Seconds until the next call can go out"))),
        (status = 504, description = "CurseForge did not answer in time, `UPSTREAM_TIMEOUT`", body = ApiError),
        (status = "default", description = "Any other status CurseForge answered, with its body. Only the gateway's own answers lack `X-Cache`"),
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
        Source::CurseForge,
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

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;

    use super::*;
    use crate::test_support::*;

    #[tokio::test]
    async fn an_answer_stays_fresh_for_the_time_of_its_route() {
        let mock = Mock::start(Duration::ZERO, |_, _| reply(200, "{}")).await;
        let gateway = gateway(&mock);
        get(&gateway.router, "/v1/curseforge/mods/1").await;
        get(&gateway.router, "/v1/curseforge/mods/search?gameId=432").await;

        gateway.clock.advance(6 * MINUTE_MS);
        assert_eq!(
            get(&gateway.router, "/v1/curseforge/mods/1")
                .await
                .header("x-cache"),
            Some("hit")
        );
        assert_eq!(
            get(&gateway.router, "/v1/curseforge/mods/search?gameId=432")
                .await
                .header("x-cache"),
            Some("miss")
        );

        gateway.clock.advance(55 * MINUTE_MS);
        assert_eq!(
            get(&gateway.router, "/v1/curseforge/mods/1")
                .await
                .header("x-cache"),
            Some("miss")
        );
        assert_eq!(mock.calls(), 4);
    }

    #[tokio::test]
    async fn not_found_is_kept_for_the_shorter_not_found_time() {
        let mock = Mock::start(Duration::ZERO, |_, _| reply(404, "")).await;
        let gateway = gateway(&mock);
        assert_eq!(
            get(&gateway.router, "/v1/curseforge/mods/1").await.status,
            StatusCode::NOT_FOUND
        );
        gateway.clock.advance(4 * MINUTE_MS);
        let again = get(&gateway.router, "/v1/curseforge/mods/1").await;
        assert_eq!(
            (again.status, again.header("x-cache")),
            (StatusCode::NOT_FOUND, Some("hit"))
        );
        gateway.clock.advance(2 * MINUTE_MS);
        assert_eq!(
            get(&gateway.router, "/v1/curseforge/mods/1")
                .await
                .header("x-cache"),
            Some("miss")
        );
        assert_eq!(mock.calls(), 2);
    }

    #[test]
    fn the_cache_table_matches_routes_by_shape() {
        let found =
            |path: &str| freshness(&path.split('/').collect::<Vec<_>>()).map(|f| f.found_ms);
        assert_eq!(found("mods/search"), Some(5 * MINUTE_MS));
        assert_eq!(found("mods/238222"), Some(HOUR_MS));
        assert_eq!(found("mods/238222/files/4567/download-url"), Some(HOUR_MS));
        assert_eq!(found("mods/238222/files/4567/changelog"), Some(DAY_MS));
        assert_eq!(found("minecraft/modloader"), Some(DAY_MS));
        assert_eq!(found("mods/jei"), None);
        assert_eq!(found("mods/238222/files/latest"), None);
        assert_eq!(found("games"), None);
    }

    #[test]
    fn a_route_with_a_query_is_not_shared_with_the_batches() {
        assert_eq!(share_of(&["mods", "1"], None), Some(Share::Mod(1)));
        assert_eq!(share_of(&["mods", "1"], Some("")), Some(Share::Mod(1)));
        assert_eq!(share_of(&["mods", "1"], Some("gameId=432")), None);
        assert_eq!(share_of(&["mods", "1", "files", "10"], Some("x=1")), None);
    }
}
