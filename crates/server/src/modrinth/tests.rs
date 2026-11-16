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
    let found = |path: &str| freshness(&path.split('/').collect::<Vec<_>>()).map(|f| f.found_ms);
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
        tokio::spawn(async move { get(&router, &format!("/v1/modrinth/version_file/{JEI}")).await })
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
