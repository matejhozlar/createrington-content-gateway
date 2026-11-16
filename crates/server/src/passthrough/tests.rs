use std::time::Duration;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};

use super::*;
use crate::{
    app::Timeouts,
    clock::{DAY_MS, HOUR_MS},
    test_support::*,
};

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

    let snapshot = gateway.state.curseforge_budget.snapshot().await;
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
async fn no_cache_asks_curseforge_again_and_keeps_the_new_answer() {
    let mock = Mock::start(Duration::ZERO, |call, _| {
        reply(200, format!("{{\"call\":{call}}}"))
    })
    .await;
    let gateway = gateway(&mock);
    get(&gateway.router, "/v1/curseforge/mods/1/files").await;

    let forced = send(
        &gateway.router,
        Request::get("/v1/curseforge/mods/1/files").header("cache-control", "max-age=0, no-cache"),
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
        gateway.state.curseforge_budget.snapshot().await.days[0].pools[0]
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
        gateway.state.curseforge_budget.snapshot().await.days[0].pools[0]
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
        gateway.state.curseforge_budget.snapshot().await.days[0].pools[0]
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
    let snapshot = gateway.state.curseforge_budget.snapshot().await;
    let counts = &snapshot.days[0].pools[0].counts;
    assert_eq!((counts.calls, counts.hits), (2, 0));

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
    let snapshot = gateway.state.curseforge_budget.snapshot().await;
    let counts = &snapshot.days[0].pools[0].counts;
    assert_eq!((counts.calls, counts.hits), (2, 1));
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
    assert_eq!(gateway.state.curseforge_budget.snapshot().await.used, 2);
}

#[tokio::test]
async fn a_route_outside_the_cache_table_is_counted_but_not_kept() {
    let mock = Mock::start(Duration::ZERO, |_, _| reply(200, "{}")).await;
    let gateway = gateway(&mock);
    get(&gateway.router, "/v1/curseforge/games").await;
    get(&gateway.router, "/v1/curseforge/games").await;
    assert_eq!(mock.calls(), 2);
    assert_eq!(gateway.state.curseforge_budget.snapshot().await.used, 2);
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
        gateway.state.curseforge_budget.snapshot().await.days[0].pools[0]
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
