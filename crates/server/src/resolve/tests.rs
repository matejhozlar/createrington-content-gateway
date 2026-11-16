use axum::http::Request;
use serde_json::{Value, json};

use super::*;
use crate::{
    app::Timeouts, config::DEFAULT_USER_AGENT, curseforge::TIMEOUT, modrinth, test_support::*,
};

const JEI_SHA1: &str = "3e1dd9fb5ea2f0dd4ffcc2b1a7d0bb1a1a9d6a8e";
const SODIUM_SHA1: &str = "0123456789abcdef0123456789abcdef01234567";
const CREATE_SHA1: &str = "abcdefabcdefabcdefabcdefabcdefabcdefabcd";

fn file(id: u64, name: &str, size: u64, sha1: &str, url: Option<&str>) -> Value {
    json!({
        "id": id, "modId": id / 10, "fileName": name, "fileLength": size,
        "downloadUrl": url,
        "hashes": [{ "value": sha1.to_ascii_uppercase(), "algo": 1 }, { "value": "md5md5", "algo": 2 }]
    })
}

fn files_answer(files: &[Value]) -> String {
    json!({ "data": files }).to_string()
}

fn modrinth_answer(found: &[(&str, &str)]) -> String {
    let mut body = serde_json::Map::new();
    for (sha1, name) in found {
        body.insert(
            (*sha1).to_owned(),
            json!({ "id": "v", "files": [
                { "hashes": { "sha1": sha1 }, "url": format!("https://cdn.modrinth.com/data/p/versions/v/{name}"), "filename": name, "size": 1 },
                { "hashes": { "sha1": "ffffffffffffffffffffffffffffffffffffffff" }, "url": "https://cdn.modrinth.com/data/p/versions/v/other.jar", "filename": "other.jar", "size": 1 }
            ] }),
        );
    }
    Value::Object(body).to_string()
}

fn cdn_reply(status: u16, length: &'static str) -> Reply {
    Reply {
        status,
        headers: vec![("content-length", length)],
        body: String::new(),
    }
}

async fn resolve(gateway: &Gateway, body: &str) -> Got {
    post_json(&gateway.router, "/v1/resolve", body).await
}

#[tokio::test]
async fn takes_curseforge_then_modrinth_then_the_cdn_and_says_when_nothing_has_a_file() {
    let mock = Mock::start(Duration::ZERO, |_, seen| match seen.uri.as_str() {
        "/v1/mods/files" => reply(
            200,
            files_answer(&[
                file(
                    1001,
                    "jei.jar",
                    10,
                    JEI_SHA1,
                    Some("https://edge.forgecdn.net/jei.jar"),
                ),
                file(1002, "sodium.jar", 20, SODIUM_SHA1, None),
                file(1003, "create 1.0(a).jar", 30, CREATE_SHA1, None),
                file(
                    1004,
                    "gone.jar",
                    40,
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    None,
                ),
            ]),
        ),
        "/v2/version_files" => reply(200, modrinth_answer(&[(SODIUM_SHA1, "sodium.jar")])),
        "/files/1/3/create%201.0%28a%29.jar" => cdn_reply(200, "30"),
        "/files/1/4/gone.jar" => cdn_reply(403, "0"),
        uri => panic!("unexpected {uri}"),
    })
    .await;
    let gateway = gateway(&mock);

    let got = resolve(&gateway, r#"{"fileIds":[1001,1002,1003,1004,9999,1001]}"#).await;
    assert_eq!(got.status, StatusCode::OK);
    assert_eq!(got.header("x-cache"), None);
    let files = got.json()["files"].clone();
    assert_eq!(files.as_array().unwrap().len(), 5);
    assert_eq!(files[0]["source"], "curseforge");
    assert_eq!(files[0]["url"], "https://edge.forgecdn.net/jei.jar");
    assert_eq!(files[0]["sha1"], JEI_SHA1);
    assert_eq!(files[0]["fileName"], "jei.jar");
    assert_eq!(files[0]["size"], 10);
    assert_eq!(files[1]["source"], "modrinth");
    assert_eq!(
        files[1]["url"],
        "https://cdn.modrinth.com/data/p/versions/v/sodium.jar"
    );
    assert_eq!(files[2]["source"], "curseforge-cdn");
    assert_eq!(
        files[2]["url"],
        format!("{}/files/1/3/create%201.0%28a%29.jar", mock.base)
    );
    assert_eq!(files[3]["source"], Value::Null);
    assert_eq!(files[3]["url"], Value::Null);
    assert_eq!(files[3]["retry"], false);
    assert_eq!(files[4]["fileId"], 9999);
    assert_eq!(files[4]["source"], Value::Null);
    assert_eq!(files[4]["sha1"], Value::Null);
    assert_eq!(files[4]["retry"], false);

    let seen = mock.seen();
    let modrinth = seen
        .iter()
        .find(|seen| seen.uri == "/v2/version_files")
        .unwrap();
    let asked: Value = serde_json::from_str(&modrinth.body).unwrap();
    assert_eq!(
        asked["hashes"],
        json!([
            SODIUM_SHA1,
            CREATE_SHA1,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        ])
    );
    let heads: Vec<&Seen> = seen.iter().filter(|seen| seen.method == "HEAD").collect();
    assert_eq!(heads.len(), 2);
    assert!(
        heads
            .iter()
            .all(|head| head.user_agent.as_deref() == Some(USER_AGENT))
    );
    assert_eq!(gateway.state.curseforge_budget.snapshot().await.used, 1);

    let again = resolve(&gateway, r#"{"fileIds":[1003,1004]}"#).await;
    assert_eq!(again.json()["files"][0]["source"], "curseforge-cdn");
    assert_eq!(mock.calls(), seen.len());
}

#[tokio::test]
async fn the_cdn_is_skipped_when_asked_and_a_wrong_size_is_no_answer() {
    let mock = Mock::start(Duration::ZERO, |_, seen| match seen.uri.as_str() {
        "/v1/mods/files" => reply(
            200,
            files_answer(&[
                file(1003, "create.jar", 30, CREATE_SHA1, None),
                file(1005, "short.jar", 50, SODIUM_SHA1, None),
            ]),
        ),
        "/v2/version_files" => reply(200, "{}"),
        "/files/1/5/short.jar" => cdn_reply(200, "49"),
        uri => panic!("unexpected {uri}"),
    })
    .await;
    let gateway = gateway(&mock);

    let off = resolve(&gateway, r#"{"fileIds":[1003],"cdn":false}"#).await;
    assert_eq!(off.json()["files"][0]["source"], Value::Null);
    assert_eq!(off.json()["files"][0]["retry"], false);
    assert!(mock.seen().iter().all(|seen| seen.method != "HEAD"));

    let wrong = resolve(&gateway, r#"{"fileIds":[1005]}"#).await;
    assert_eq!(wrong.json()["files"][0]["source"], Value::Null);
    assert_eq!(wrong.json()["files"][0]["retry"], false);
    resolve(&gateway, r#"{"fileIds":[1005]}"#).await;
    assert_eq!(
        mock.seen()
            .iter()
            .filter(|seen| seen.method == "HEAD")
            .count(),
        1
    );
}

#[tokio::test]
async fn a_modrinth_failure_asks_the_cdn_and_marks_the_rest_for_a_retry() {
    let mock = Mock::start(Duration::ZERO, |_, seen| match seen.uri.as_str() {
        "/v1/mods/files" => reply(
            200,
            files_answer(&[
                file(1003, "create.jar", 30, CREATE_SHA1, None),
                file(1006, "lost.jar", 60, SODIUM_SHA1, None),
            ]),
        ),
        "/v2/version_files" => reply(500, "{}"),
        "/files/1/3/create.jar" => cdn_reply(200, "30"),
        "/files/1/6/lost.jar" => cdn_reply(404, "0"),
        uri => panic!("unexpected {uri}"),
    })
    .await;
    let gateway = gateway(&mock);

    let got = resolve(&gateway, r#"{"fileIds":[1003,1006]}"#).await;
    assert_eq!(got.status, StatusCode::OK);
    assert_eq!(got.json()["files"][0]["source"], "curseforge-cdn");
    assert_eq!(got.json()["files"][0]["retry"], false);
    assert_eq!(got.json()["files"][1]["source"], Value::Null);
    assert_eq!(got.json()["files"][1]["retry"], true);
}

#[tokio::test]
async fn a_curseforge_failure_is_the_passthrough_s_answer() {
    let mock = Mock::start(Duration::ZERO, |_, _| reply(500, "{}")).await;
    let failing = gateway(&mock);
    let got = resolve(&failing, r#"{"fileIds":[1003]}"#).await;
    assert_eq!(got.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(got.header("x-cache"), Some("miss"));

    let refused = gateway_with(
        &mock,
        GatewayWith {
            limit: 1,
            ..GatewayWith::default()
        },
    );
    resolve(&refused, r#"{"fileIds":[1003]}"#).await;
    let got = resolve(&refused, r#"{"fileIds":[1004]}"#).await;
    assert_eq!(got.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(got.json()["code"], "BUDGET_SPENT");
}

#[tokio::test]
async fn refuses_a_bad_body() {
    let mock = Mock::start(Duration::ZERO, |_, _| reply(200, "{}")).await;
    let gateway = gateway(&mock);
    for body in ["{}", r#"{"fileIds":[]}"#, r#"{"fileIds":["x"]}"#, "nope"] {
        let got = resolve(&gateway, body).await;
        assert_eq!(got.status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(got.json()["code"], "INVALID_BODY", "{body}");
    }
    let many: Vec<String> = (0..1001).map(|id| id.to_string()).collect();
    let got = resolve(&gateway, &format!(r#"{{"fileIds":[{}]}}"#, many.join(","))).await;
    assert_eq!(got.status, StatusCode::BAD_REQUEST);
    assert_eq!(got.json()["code"], "INVALID_BODY");
    assert_eq!(mock.calls(), 0);
}

#[test]
fn the_longest_call_is_the_three_sources_in_a_row() {
    assert_eq!(
        TIMEOUT + modrinth::TIMEOUT + CDN_WAIT,
        Duration::from_secs(40)
    );
}

#[tokio::test]
async fn a_file_without_a_name_or_size_keeps_its_link_and_skips_the_cdn() {
    let mock = Mock::start(Duration::ZERO, |_, seen| match seen.uri.as_str() {
        "/v1/mods/files" => reply(
            200,
            json!({ "data": [
                { "id": 1007, "modId": 100, "downloadUrl": "https://edge.forgecdn.net/bare.jar", "hashes": [] },
                { "id": 1008, "modId": 100, "fileLength": 80, "hashes": [{ "value": CREATE_SHA1, "algo": 1 }] },
            ] })
            .to_string(),
        ),
        "/v2/version_files" => reply(200, "{}"),
        uri => panic!("unexpected {uri}"),
    })
    .await;
    let gateway = gateway(&mock);

    let got = resolve(&gateway, r#"{"fileIds":[1007,1008]}"#).await;
    let files = got.json()["files"].clone();
    assert_eq!(files[0]["source"], "curseforge");
    assert_eq!(files[0]["fileName"], Value::Null);
    assert_eq!(files[0]["size"], Value::Null);
    assert_eq!(files[1]["source"], Value::Null);
    assert_eq!(files[1]["sha1"], CREATE_SHA1);
    assert_eq!(files[1]["size"], 80);
    assert_eq!(files[1]["retry"], false);
    assert!(mock.seen().iter().all(|seen| seen.method != "HEAD"));
}

#[test]
fn builds_the_cdn_address_like_curseforge_does() {
    let cdn = Cdn::new(CURSEFORGE_CDN, DEFAULT_USER_AGENT, CDN_TIMEOUT, CDN_WAIT).unwrap();
    assert_eq!(
        cdn.url(4567890, "Create-1.20.1 (v0.5.1f)!'*.jar"),
        "https://mediafilez.forgecdn.net/files/4567/890/Create-1.20.1%20%28v0.5.1f%29%21%27%2A.jar"
    );
    assert_eq!(
        cdn.url(999, "ü.jar"),
        "https://mediafilez.forgecdn.net/files/0/999/%C3%BC.jar"
    );
}

fn only_on_the_cdn(head: Reply) -> impl Fn(usize, &Seen) -> Reply + Send + Sync + 'static {
    let (status, headers) = (head.status, head.headers);
    move |_, seen| match seen.uri.as_str() {
        "/v1/mods/files" => reply(
            200,
            files_answer(&[file(1003, "create.jar", 30, CREATE_SHA1, None)]),
        ),
        "/v2/version_files" => reply(200, "{}"),
        "/files/1/3/create.jar" => Reply {
            status,
            headers: headers.clone(),
            body: String::new(),
        },
        uri => panic!("unexpected {uri}"),
    }
}

fn heads(mock: &Mock) -> usize {
    mock.seen()
        .iter()
        .filter(|seen| seen.method == "HEAD")
        .count()
}

#[tokio::test]
async fn a_curseforge_that_does_not_answer_in_time_is_a_504() {
    let (mock, _never) = Mock::held(only_on_the_cdn(cdn_reply(200, "30"))).await;
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
    let got = resolve(&gateway, r#"{"fileIds":[1003]}"#).await;
    assert_eq!(got.status, StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(got.json()["code"], "UPSTREAM_TIMEOUT");
}

#[tokio::test]
async fn a_cdn_error_asks_for_a_retry_and_is_not_kept() {
    let mock = Mock::start(Duration::ZERO, only_on_the_cdn(cdn_reply(503, "0"))).await;
    let gateway = gateway(&mock);
    for asked in 1..=2 {
        let got = resolve(&gateway, r#"{"fileIds":[1003]}"#).await;
        let file = &got.json()["files"][0];
        assert_eq!(file["source"], Value::Null);
        assert_eq!(file["retry"], true);
        assert_eq!(heads(&mock), asked);
    }
}

#[tokio::test]
async fn a_cdn_that_does_not_answer_asks_for_a_retry() {
    let (mock, _never) = Mock::held_when(
        |seen| seen.method == "HEAD",
        only_on_the_cdn(cdn_reply(200, "30")),
    )
    .await;
    let gateway = gateway_with(
        &mock,
        GatewayWith {
            timeouts: Timeouts {
                cdn: Duration::from_millis(50),
                ..Timeouts::default()
            },
            ..GatewayWith::default()
        },
    );
    let got = resolve(&gateway, r#"{"fileIds":[1003]}"#).await;
    let file = &got.json()["files"][0];
    assert_eq!(file["source"], Value::Null);
    assert_eq!(file["retry"], true);
}

#[tokio::test]
async fn a_check_of_the_same_file_joins_the_one_running() {
    let (mock, gate) = Mock::held_when(
        |seen| seen.method == "HEAD",
        only_on_the_cdn(cdn_reply(200, "30")),
    )
    .await;
    let gateway = gateway(&mock);
    let first = tokio::spawn({
        let router = gateway.router.clone();
        async move { post_json(&router, "/v1/resolve", r#"{"fileIds":[1003]}"#).await }
    });
    until("the first check to reach the CDN", || heads(&mock) == 1).await;
    let second = tokio::spawn({
        let router = gateway.router.clone();
        async move { post_json(&router, "/v1/resolve", r#"{"fileIds":[1003]}"#).await }
    });
    until("the second check to join it", || {
        gateway.state.cache.joined() == 1
    })
    .await;
    gate.open();

    for got in [first.await.unwrap(), second.await.unwrap()] {
        let file = &got.json()["files"][0];
        assert_eq!(file["source"], "curseforge-cdn");
        assert_eq!(file["retry"], false);
    }
    assert_eq!(heads(&mock), 1);
}

#[test]
fn a_kept_cdn_verdict_reads_back_as_it_was_given() {
    for verdict in [
        CdnVerdict::Found("https://mediafilez.forgecdn.net/files/1/3/a.jar".to_owned()),
        CdnVerdict::Missing,
    ] {
        let entry = verdict.entry(NOV_3_2026_20H).unwrap();
        assert_eq!(CdnVerdict::kept(&entry), verdict);
    }
    assert!(CdnVerdict::Unknown.entry(NOV_3_2026_20H).is_none());
}

#[tokio::test]
async fn a_check_outlives_the_request_that_started_it_and_is_kept_for_the_retry() {
    let (mock, gate) = Mock::held_when(
        |seen| seen.method == "HEAD",
        only_on_the_cdn(cdn_reply(200, "30")),
    )
    .await;
    let gateway = gateway_with(
        &mock,
        GatewayWith {
            timeouts: Timeouts {
                cdn_wait: Duration::from_millis(50),
                ..Timeouts::default()
            },
            ..GatewayWith::default()
        },
    );
    let gave_up = resolve(&gateway, r#"{"fileIds":[1003]}"#).await;
    assert_eq!(gave_up.json()["files"][0]["retry"], true);

    gate.open();
    until("the check to finish", || {
        gateway.state.cache.loads_running() == 0
    })
    .await;
    let retried = resolve(&gateway, r#"{"fileIds":[1003]}"#).await;
    let file = &retried.json()["files"][0];
    assert_eq!(file["source"], "curseforge-cdn");
    assert_eq!(file["retry"], false);
    assert_eq!(heads(&mock), 1);
}

#[tokio::test]
async fn checks_still_running_when_the_cdn_wait_ends_ask_for_a_retry() {
    let (mock, _never) = Mock::held_when(
        |seen| seen.method == "HEAD",
        only_on_the_cdn(cdn_reply(200, "30")),
    )
    .await;
    let gateway = gateway_with(
        &mock,
        GatewayWith {
            timeouts: Timeouts {
                cdn_wait: Duration::from_millis(50),
                ..Timeouts::default()
            },
            ..GatewayWith::default()
        },
    );
    let got = tokio::time::timeout(
        Timeouts::default().cdn / 2,
        resolve(&gateway, r#"{"fileIds":[1003]}"#),
    )
    .await
    .expect("the wait ended the check before the CDN client gave up");
    assert_eq!(got.status, StatusCode::OK);
    let file = &got.json()["files"][0];
    assert_eq!(file["source"], Value::Null);
    assert_eq!(file["retry"], true);
}

#[tokio::test]
async fn a_modrinth_link_on_another_host_is_not_given_out() {
    let mock = Mock::start(Duration::ZERO, |_, seen| match seen.uri.as_str() {
        "/v1/mods/files" => reply(
            200,
            files_answer(&[file(1003, "create.jar", 30, CREATE_SHA1, None)]),
        ),
        "/v2/version_files" => reply(
            200,
            json!({ CREATE_SHA1: { "id": "v", "files": [
                { "hashes": { "sha1": CREATE_SHA1 }, "url": "https://elsewhere.example.com/create.jar", "filename": "create.jar", "size": 30 },
                { "hashes": { "sha1": CREATE_SHA1 }, "url": "http://cdn.modrinth.com/data/p/versions/v/create.jar", "filename": "create.jar", "size": 30 }
            ] } })
            .to_string(),
        ),
        "/files/1/3/create.jar" => cdn_reply(200, "30"),
        uri => panic!("unexpected {uri}"),
    })
    .await;
    let gateway = gateway(&mock);
    let got = resolve(&gateway, r#"{"fileIds":[1003]}"#).await;
    let file = &got.json()["files"][0];
    assert_eq!(file["source"], "curseforge-cdn");
    assert_eq!(file["url"], format!("{}/files/1/3/create.jar", mock.base));
}

#[tokio::test]
async fn no_cache_asks_every_source_again() {
    let mock = Mock::start(Duration::ZERO, only_on_the_cdn(cdn_reply(200, "30"))).await;
    let gateway = gateway(&mock);
    resolve(&gateway, r#"{"fileIds":[1003]}"#).await;
    let kept = resolve(&gateway, r#"{"fileIds":[1003]}"#).await;
    assert_eq!(kept.json()["files"][0]["source"], "curseforge-cdn");
    assert_eq!(mock.calls(), 3);

    let forced = send(
        &gateway.router,
        Request::post("/v1/resolve")
            .header("content-type", "application/json")
            .header("cache-control", "no-cache"),
        r#"{"fileIds":[1003]}"#,
    )
    .await;
    assert_eq!(forced.json()["files"][0]["source"], "curseforge-cdn");
    let asked: Vec<String> = mock.seen()[3..]
        .iter()
        .map(|seen| format!("{} {}", seen.method, seen.uri))
        .collect();
    assert_eq!(
        asked,
        [
            "POST /v1/mods/files",
            "POST /v2/version_files",
            "HEAD /files/1/3/create.jar"
        ]
    );
}
