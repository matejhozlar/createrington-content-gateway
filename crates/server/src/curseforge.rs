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
    error::ApiError,
    passthrough::{self, DAY_MS, Freshness, HOUR_MS, MINUTE_MS, fresh, is_id},
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
/// cache, and only the ids not kept are asked for, 100 per call. Every other body goes to CurseForge each time.
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
        (status = 400, description = "`INVALID_PATH`, `POOL_REQUIRED` or `UNKNOWN_POOL`", body = ApiError),
        (status = 401, description = "No valid consumer token", body = ApiError),
        (status = 413, description = "The batch needs more calls than a whole minute allows, `REQUEST_TOO_LARGE`; split it", body = ApiError),
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
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };

    use super::*;
    use crate::{app::Timeouts, test_support::*};

    #[tokio::test]
    async fn passes_an_answer_through_as_it_came() {
        let mock = Mock::start(Duration::ZERO, |_, _| {
            reply(
                200,
                r#"{ "data" : [ {"id":1,"name":"JEI"} ], "pagination": {} }"#,
            )
        })
        .await;
        let gateway = gateway(&mock);

        let got = get(
            &gateway.router,
            "/v1/curseforge/mods/search?gameId=432&searchFilter=jei%20mod",
        )
        .await;
        assert_eq!(got.status, StatusCode::OK);
        assert_eq!(
            got.body,
            r#"{ "data" : [ {"id":1,"name":"JEI"} ], "pagination": {} }"#
        );
        assert_eq!(
            got.header("content-type"),
            Some("application/json; charset=utf-8")
        );
        assert_eq!(got.header("x-cache"), Some("miss"));

        let seen = mock.seen();
        assert_eq!(seen[0].method, "GET");
        assert_eq!(
            seen[0].uri,
            "/v1/mods/search?gameId=432&searchFilter=jei%20mod"
        );
        assert_eq!(seen[0].key.as_deref(), Some(KEY));
        assert_eq!(seen[0].user_agent.as_deref(), Some(USER_AGENT));
    }

    #[tokio::test]
    async fn a_fresh_answer_comes_from_the_cache_and_counts_as_a_hit() {
        let mock = Mock::start(Duration::ZERO, |_, _| reply(200, r#"{"data":{"id":1}}"#)).await;
        let gateway = gateway(&mock);

        assert_eq!(
            get(&gateway.router, "/v1/curseforge/mods/1")
                .await
                .header("x-cache"),
            Some("miss")
        );
        let again = get(&gateway.router, "/v1/curseforge/mods/1").await;
        assert_eq!(again.header("x-cache"), Some("hit"));
        assert_eq!(again.body, r#"{"data":{"id":1}}"#);
        assert_eq!(mock.calls(), 1);

        let snapshot = gateway.state.budget.snapshot().await;
        assert_eq!(snapshot.used, 1);
        assert_eq!(snapshot.days[0].pools[0].counts.calls, 1);
        assert_eq!(snapshot.days[0].pools[0].counts.hits, 1);
    }

    #[tokio::test]
    async fn the_order_of_query_parameters_does_not_matter_to_the_cache() {
        let mock = Mock::start(Duration::ZERO, |_, _| reply(200, "{}")).await;
        let gateway = gateway(&mock);
        get(
            &gateway.router,
            "/v1/curseforge/mods/search?gameId=432&classId=6",
        )
        .await;
        let again = get(
            &gateway.router,
            "/v1/curseforge/mods/search?classId=6&gameId=432",
        )
        .await;
        assert_eq!(again.header("x-cache"), Some("hit"));
        get(
            &gateway.router,
            "/v1/curseforge/mods/search?gameId=432&classId=12",
        )
        .await;
        assert_eq!(mock.calls(), 2);
    }

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

    #[tokio::test]
    async fn no_cache_asks_curseforge_again_and_keeps_the_new_answer() {
        let mock = Mock::start(Duration::ZERO, |call, _| {
            reply(200, format!("{{\"call\":{call}}}"))
        })
        .await;
        let gateway = gateway(&mock);
        get(&gateway.router, "/v1/curseforge/mods/1/files").await;

        let forced = send(
            &gateway.router,
            Request::get("/v1/curseforge/mods/1/files")
                .header("cache-control", "max-age=0, no-cache"),
            "",
        )
        .await;
        assert_eq!(forced.header("x-cache"), Some("miss"));
        assert_eq!(forced.body, r#"{"call":1}"#);

        let after = get(&gateway.router, "/v1/curseforge/mods/1/files").await;
        assert_eq!(after.header("x-cache"), Some("hit"));
        assert_eq!(after.body, r#"{"call":1}"#);
        assert_eq!(mock.calls(), 2);
    }

    #[tokio::test]
    async fn concurrent_misses_make_one_call() {
        let (mock, gate) = Mock::held(|_, _| reply(200, "{}")).await;
        let gateway = gateway(&mock);
        let requests: Vec<_> = (0..5)
            .map(|_| {
                let router = gateway.router.clone();
                tokio::spawn(async move { get(&router, "/v1/curseforge/mods/1").await })
            })
            .collect();
        until("four requests joining the first", || {
            gateway.state.cache.joined() == 4
        })
        .await;
        gate.open();
        let mut caches = Vec::new();
        for request in requests {
            caches.push(request.await.unwrap().header("x-cache").unwrap().to_owned());
        }
        caches.sort();
        assert_eq!(caches, ["hit", "hit", "hit", "hit", "miss"]);
        assert_eq!(mock.calls(), 1);
        assert_eq!(
            gateway.state.budget.snapshot().await.days[0].pools[0]
                .counts
                .hits,
            4
        );
    }

    #[tokio::test]
    async fn a_call_finishes_and_is_recorded_after_its_caller_is_gone() {
        let (mock, gate) = Mock::held(|_, _| reply(200, "{}")).await;
        let gateway = gateway(&mock);
        let caller = {
            let router = gateway.router.clone();
            tokio::spawn(async move { get(&router, "/v1/curseforge/mods/1").await })
        };
        until("the call to reach CurseForge", || mock.calls() == 1).await;
        caller.abort();
        gate.open();
        until("the load to finish", || {
            gateway.state.cache.loads_running() == 0
        })
        .await;

        assert_eq!(mock.calls(), 1);
        assert_eq!(gateway.state.cache.loads_running(), 0);
        assert_eq!(
            gateway.state.budget.snapshot().await.days[0].pools[0]
                .counts
                .calls,
            1
        );
        assert_eq!(
            get(&gateway.router, "/v1/curseforge/mods/1")
                .await
                .header("x-cache"),
            Some("hit")
        );
    }

    #[tokio::test]
    async fn no_cache_does_not_join_a_load_that_may_not_ask_curseforge() {
        let (mock, gate) = Mock::held(|_, _| reply(200, "{}")).await;
        let gateway = gateway(&mock);
        let plain = {
            let router = gateway.router.clone();
            tokio::spawn(async move { get(&router, "/v1/curseforge/mods/1").await })
        };
        until("the plain call to reach CurseForge", || mock.calls() == 1).await;
        let forced = {
            let router = gateway.router.clone();
            tokio::spawn(async move {
                send(
                    &router,
                    Request::get("/v1/curseforge/mods/1").header("cache-control", "no-cache"),
                    "",
                )
                .await
            })
        };
        until("the forced call to reach CurseForge", || mock.calls() == 2).await;
        gate.open();

        assert_eq!(plain.await.unwrap().header("x-cache"), Some("miss"));
        assert_eq!(forced.await.unwrap().header("x-cache"), Some("miss"));
        assert_eq!(gateway.state.cache.joined(), 0);
    }

    #[tokio::test]
    async fn a_429_holds_every_route_off() {
        let mock = Mock::start(Duration::ZERO, |_, _| Reply {
            status: 429,
            headers: vec![("retry-after", "30")],
            body: String::new(),
        })
        .await;
        let gateway = gateway(&mock);

        let throttled = get(&gateway.router, "/v1/curseforge/mods/1").await;
        assert_eq!(throttled.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(throttled.header("retry-after"), Some("30"));
        assert_eq!(throttled.json()["code"], "UPSTREAM_HOLD");

        let held = get(&gateway.router, "/v1/curseforge/categories?gameId=432").await;
        assert_eq!(held.json()["code"], "UPSTREAM_HOLD");
        assert_eq!(held.json()["pool"], "launcher");
        assert_eq!(mock.calls(), 1);
        assert_eq!(
            gateway.state.budget.snapshot().await.days[0].pools[0]
                .counts
                .throttled,
            1
        );
    }

    #[tokio::test]
    async fn a_stale_answer_is_served_when_curseforge_fails() {
        let mock = Mock::start(Duration::ZERO, |call, _| match call {
            0 => reply(200, r#"{"data":"old"}"#),
            _ => reply(502, "bad gateway"),
        })
        .await;
        let gateway = gateway(&mock);
        get(&gateway.router, "/v1/curseforge/mods/1").await;
        gateway.clock.advance(2 * HOUR_MS);

        let stale = get(&gateway.router, "/v1/curseforge/mods/1").await;
        assert_eq!(stale.status, StatusCode::OK);
        assert_eq!(stale.header("x-cache"), Some("stale"));
        assert_eq!(stale.body, r#"{"data":"old"}"#);
        let snapshot = gateway.state.budget.snapshot().await;
        let counts = &snapshot.days[0].pools[0].counts;
        assert_eq!((counts.calls, counts.hits), (2, 1));

        gateway.clock.advance(DAY_MS);
        let gone = get(&gateway.router, "/v1/curseforge/mods/1").await;
        assert_eq!(
            (gone.status, gone.body),
            (StatusCode::BAD_GATEWAY, Bytes::from("bad gateway"))
        );
    }

    #[tokio::test]
    async fn a_stale_answer_is_served_when_the_budget_refuses() {
        let mock = Mock::start(Duration::ZERO, |_, _| reply(200, "{}")).await;
        let gateway = gateway_with(
            &mock,
            GatewayWith {
                limit: 1,
                ..GatewayWith::default()
            },
        );
        get(&gateway.router, "/v1/curseforge/mods/1").await;
        gateway.clock.advance(2 * HOUR_MS);
        get(&gateway.router, "/v1/curseforge/mods/2").await;

        let stale = get(&gateway.router, "/v1/curseforge/mods/1").await;
        assert_eq!(stale.header("x-cache"), Some("stale"));
        let refused = get(&gateway.router, "/v1/curseforge/mods/3").await;
        assert_eq!(refused.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(refused.json()["code"], "BUDGET_SPENT");
        assert_eq!(mock.calls(), 2);
    }

    #[tokio::test]
    async fn an_error_answer_is_passed_on_and_not_kept() {
        let mock = Mock::start(Duration::ZERO, |_, _| reply(500, r#"{"error":"boom"}"#)).await;
        let gateway = gateway(&mock);
        let first = get(&gateway.router, "/v1/curseforge/mods/1").await;
        assert_eq!(
            (first.status, first.body),
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Bytes::from(r#"{"error":"boom"}"#)
            )
        );
        get(&gateway.router, "/v1/curseforge/mods/1").await;
        assert_eq!(mock.calls(), 2);
    }

    #[tokio::test]
    async fn a_post_that_is_not_a_plain_batch_goes_through_with_its_body() {
        let mock = Mock::start(Duration::ZERO, |_, _| reply(200, r#"{"data":[]}"#)).await;
        let gateway = gateway(&mock);
        for _ in 0..2 {
            let got = send(
                &gateway.router,
                Request::post("/v1/curseforge/mods"),
                r#"{"modIds":[1,2],"filterPcOnly":true}"#,
            )
            .await;
            assert_eq!(got.body, r#"{"data":[]}"#);
        }
        let seen = mock.seen();
        assert_eq!(
            (seen[0].method.as_str(), seen[0].uri.as_str()),
            ("POST", "/v1/mods")
        );
        assert_eq!(seen[0].body, r#"{"modIds":[1,2],"filterPcOnly":true}"#);
        assert_eq!(gateway.state.budget.snapshot().await.used, 2);
    }

    #[tokio::test]
    async fn a_route_outside_the_cache_table_is_counted_but_not_kept() {
        let mock = Mock::start(Duration::ZERO, |_, _| reply(200, "{}")).await;
        let gateway = gateway(&mock);
        get(&gateway.router, "/v1/curseforge/games").await;
        get(&gateway.router, "/v1/curseforge/games").await;
        assert_eq!(mock.calls(), 2);
        assert_eq!(gateway.state.budget.snapshot().await.used, 2);
    }

    #[tokio::test]
    async fn refuses_a_path_that_could_leave_the_v1_api() {
        let mock = Mock::start(Duration::ZERO, |_, _| reply(200, "{}")).await;
        let gateway = gateway(&mock);
        for uri in [
            "/v1/curseforge/..%2Fv2%2Fmods",
            "/v1/curseforge/mods/1%3Fpage=2",
            "/v1/curseforge/mods//1",
            "/v1/curseforge/mods/1/",
        ] {
            let got = get(&gateway.router, uri).await;
            assert_eq!(got.status, StatusCode::BAD_REQUEST, "{uri}");
            assert_eq!(got.json()["code"], "INVALID_PATH", "{uri}");
        }
        assert_eq!(mock.calls(), 0);
    }

    #[tokio::test]
    async fn a_curseforge_that_does_not_answer_in_time_is_a_504() {
        let (mock, _never) = Mock::held(|_, _| reply(200, "{}")).await;
        let gateway = gateway_with(
            &mock,
            GatewayWith {
                timeouts: Timeouts {
                    curseforge: Duration::from_millis(50),
                    ..Timeouts::default()
                },
                ..GatewayWith::default()
            },
        );
        let got = get(&gateway.router, "/v1/curseforge/mods/1").await;
        assert_eq!(got.status, StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(got.json()["code"], "UPSTREAM_TIMEOUT");
        assert_eq!(
            gateway.state.budget.snapshot().await.days[0].pools[0]
                .counts
                .calls,
            1
        );
    }

    #[tokio::test]
    async fn needs_a_token() {
        let mock = Mock::start(Duration::ZERO, |_, _| reply(200, "{}")).await;
        let gateway = gateway(&mock);
        let got = call(
            &gateway.router,
            Request::get("/v1/curseforge/mods/1")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(got.status, StatusCode::UNAUTHORIZED);
        assert_eq!(mock.calls(), 0);
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

    #[tokio::test]
    async fn a_curseforge_that_cannot_be_reached_is_a_502() {
        let router = crate::app::router(test_state(vec![consumer("app", TOKEN, &["launcher"])]));
        let got = get(&router, "/v1/curseforge/mods/1").await;
        assert_eq!(got.status, StatusCode::BAD_GATEWAY);
        assert_eq!(got.json()["code"], "UPSTREAM_UNREACHABLE");
        assert_eq!(got.header("x-cache"), None);
    }

    #[tokio::test]
    async fn an_answer_larger_than_the_gateway_keeps_is_a_502_and_not_kept() {
        let mock = Mock::start(Duration::ZERO, |_, _| {
            reply(200, "x".repeat(32 * 1024 * 1024 + 1))
        })
        .await;
        let gateway = gateway(&mock);
        for _ in 0..2 {
            let got = get(&gateway.router, "/v1/curseforge/mods/1").await;
            assert_eq!(got.status, StatusCode::BAD_GATEWAY);
            assert_eq!(got.json()["code"], "UPSTREAM_TOO_LARGE");
        }
        assert_eq!(mock.calls(), 2);
    }

    #[test]
    fn a_route_with_a_query_is_not_shared_with_the_batches() {
        assert_eq!(share_of(&["mods", "1"], None), Some(Share::Mod(1)));
        assert_eq!(share_of(&["mods", "1"], Some("")), Some(Share::Mod(1)));
        assert_eq!(share_of(&["mods", "1"], Some("gameId=432")), None);
        assert_eq!(share_of(&["mods", "1", "files", "10"], Some("x=1")), None);
    }
}
