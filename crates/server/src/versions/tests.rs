use serde_json::{Value, json};

use super::{
    java::{JAVA_RUNTIMES_PATH, RuntimeIndex, Wanted, pick, runtime_major},
    loaders::{
        FABRIC_GAME_PATH, FABRIC_LOADER_PATH, FORGE_PATH, NEOFORGE_PATH, neoforge_minecraft,
    },
    minecraft::MANIFEST_PATH,
    *,
};
use crate::{app::Timeouts, test_support::*};

fn manifest_body(base: &str) -> String {
    json!({
        "latest": { "release": "1.21.1", "snapshot": "24w33a" },
        "versions": [
            { "id": "24w33a", "type": "snapshot", "url": format!("{base}/v1/packages/a/24w33a.json"), "time": "x", "releaseTime": "2024-08-15T00:00:00+00:00", "sha1": "a", "complianceLevel": 1 },
            { "id": "1.21.1", "type": "release", "url": format!("{base}/v1/packages/b/1.21.1.json"), "time": "x", "releaseTime": "2024-08-08T00:00:00+00:00", "sha1": "b", "complianceLevel": 1 },
            { "id": "1.21", "type": "release", "url": format!("{base}/v1/packages/c/1.21.json"), "time": "x", "releaseTime": "2024-06-13T00:00:00+00:00", "sha1": "c", "complianceLevel": 1 },
            { "id": "1.20.1", "type": "release", "url": format!("{base}/v1/packages/d/1.20.1.json"), "time": "x", "releaseTime": "2023-06-12T00:00:00+00:00", "sha1": "d", "complianceLevel": 1 },
            { "id": "b1.7.3", "type": "old_beta", "url": format!("{base}/v1/packages/e/b1.7.3.json"), "time": "x", "releaseTime": "2011-07-11T00:00:00+00:00", "sha1": "e", "complianceLevel": 0 }
        ]
    })
    .to_string()
}

fn sources(call: usize, seen: &Seen) -> Reply {
    let base = seen.base.clone();
    match seen.uri.as_str() {
        MANIFEST_PATH => reply(200, manifest_body(&base)),
        "/v1/packages/b/1.21.1.json" => reply(
            200,
            r#"{"id":"1.21.1","javaVersion":{"component":"java-runtime-delta","majorVersion":21}}"#,
        ),
        "/v1/packages/e/b1.7.3.json" => reply(200, r#"{"id":"b1.7.3"}"#),
        NEOFORGE_PATH => reply(
            200,
            r#"{"isSnapshot":false,"versions":["20.4.80","21.0.0-beta","21.0.167","21.1.57","21.1.72"]}"#,
        ),
        FORGE_PATH => reply(
            200,
            r#"{"homepage":"x","promos":{"1.20.1-latest":"47.3.0","1.20.1-recommended":"47.2.0","1.21.1-latest":"52.0.1","1.21.1-recommended":"52.0.1"}}"#,
        ),
        FABRIC_LOADER_PATH => reply(
            200,
            r#"[{"version":"0.16.5","stable":true},{"version":"0.16.4","stable":true},{"version":"0.16.0-rc.1","stable":false}]"#,
        ),
        FABRIC_GAME_PATH => reply(
            200,
            r#"[{"version":"24w33a","stable":false},{"version":"1.21.1","stable":true},{"version":"1.21","stable":true}]"#,
        ),
        uri => {
            let _ = call;
            panic!("unexpected {uri}")
        }
    }
}

fn ids(body: &Value) -> Vec<&str> {
    body["versions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|version| {
            version["id"]
                .as_str()
                .or_else(|| version["version"].as_str())
                .unwrap()
        })
        .collect()
}

#[tokio::test]
async fn lists_releases_by_default_snapshots_or_everything_on_request_and_keeps_the_manifest() {
    let mock = Mock::start(Duration::ZERO, sources).await;
    let gateway = gateway(&mock);

    let got = get(&gateway.router, "/v1/minecraft/versions").await;
    assert_eq!(got.status, StatusCode::OK);
    let body = got.json();
    assert_eq!(body["latest"]["release"], "1.21.1");
    assert_eq!(body["latest"]["snapshot"], "24w33a");
    assert_eq!(ids(&body), ["1.21.1", "1.21", "1.20.1"]);
    assert_eq!(body["versions"][0]["type"], "release");
    assert_eq!(
        body["versions"][0]["releaseTime"],
        "2024-08-08T00:00:00+00:00"
    );

    let snapshots = get(&gateway.router, "/v1/minecraft/versions?type=snapshot").await;
    assert_eq!(ids(&snapshots.json()), ["24w33a"]);
    let all = get(&gateway.router, "/v1/minecraft/versions?type=all").await;
    assert_eq!(ids(&all.json()).len(), 5);
    assert_eq!(mock.calls(), 1);

    gateway.clock.advance(LIST_FRESH_MS + 1);
    get(&gateway.router, "/v1/minecraft/versions").await;
    assert_eq!(mock.calls(), 2);

    let bad = get(&gateway.router, "/v1/minecraft/versions?type=beta").await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    assert_eq!(bad.json()["code"], "INVALID_QUERY");
    assert_eq!(gateway.state.curseforge_budget.snapshot().await.days, []);
}

#[tokio::test]
async fn narrows_the_versions_to_the_ones_a_loader_builds_for() {
    let mock = Mock::start(Duration::ZERO, sources).await;
    let gateway = gateway(&mock);

    let neoforge = get(&gateway.router, "/v1/minecraft/versions?loader=neoforge").await;
    assert_eq!(ids(&neoforge.json()), ["1.21.1", "1.21"]);
    let forge = get(&gateway.router, "/v1/minecraft/versions?loader=forge").await;
    assert_eq!(ids(&forge.json()), ["1.21.1", "1.20.1"]);
    let fabric = get(
        &gateway.router,
        "/v1/minecraft/versions?loader=fabric&type=all",
    )
    .await;
    assert_eq!(ids(&fabric.json()), ["24w33a", "1.21.1", "1.21"]);
    assert_eq!(mock.calls(), 4);

    let any_case = get(&gateway.router, "/v1/minecraft/versions?loader=NeoForge").await;
    assert_eq!(ids(&any_case.json()), ["1.21.1", "1.21"]);
    let unknown = get(&gateway.router, "/v1/minecraft/versions?loader=quilt").await;
    assert_eq!(unknown.json()["code"], "INVALID_QUERY");
}

#[tokio::test]
async fn a_version_tells_the_java_it_needs_and_is_kept_a_day() {
    let mock = Mock::start(Duration::ZERO, sources).await;
    let gateway = gateway(&mock);

    let got = get(&gateway.router, "/v1/minecraft/versions/1.21.1").await;
    assert_eq!(got.status, StatusCode::OK);
    assert_eq!(
        got.json(),
        json!({ "id": "1.21.1", "type": "release", "releaseTime": "2024-08-08T00:00:00+00:00", "javaMajor": 21, "javaComponent": "java-runtime-delta" })
    );
    get(&gateway.router, "/v1/minecraft/versions/1.21.1").await;
    assert_eq!(mock.calls(), 2);

    let old = get(&gateway.router, "/v1/minecraft/versions/b1.7.3").await;
    assert_eq!(old.json()["javaMajor"], Value::Null);
    assert_eq!(old.json()["javaComponent"], Value::Null);

    let unknown = get(&gateway.router, "/v1/minecraft/versions/1.99").await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    assert_eq!(unknown.json()["code"], "UNKNOWN_VERSION");
}

#[tokio::test]
async fn lists_a_loader_s_builds_newest_first_and_narrows_them_to_a_minecraft_version() {
    let mock = Mock::start(Duration::ZERO, sources).await;
    let gateway = gateway(&mock);

    let neoforge = get(&gateway.router, "/v1/minecraft/loaders/neoforge/versions").await;
    assert_eq!(neoforge.status, StatusCode::OK);
    let body = neoforge.json();
    assert_eq!(body["loader"], "neoforge");
    assert_eq!(body["minecraft"], Value::Null);
    assert_eq!(
        ids(&body),
        ["21.1.72", "21.1.57", "21.0.167", "21.0.0-beta", "20.4.80"]
    );
    assert_eq!(body["versions"][0]["minecraft"], "1.21.1");
    assert_eq!(body["versions"][2]["minecraft"], "1.21");
    assert_eq!(body["versions"][3]["stable"], false);

    let narrowed = get(
        &gateway.router,
        "/v1/minecraft/loaders/NeoForge/versions?minecraft=1.21.1",
    )
    .await;
    assert_eq!(ids(&narrowed.json()), ["21.1.72", "21.1.57"]);
    assert_eq!(narrowed.json()["minecraft"], "1.21.1");

    let forge = get(
        &gateway.router,
        "/v1/minecraft/loaders/forge/versions?minecraft=1.20.1",
    )
    .await;
    let body = forge.json();
    assert_eq!(ids(&body), ["47.3.0", "47.2.0"]);
    assert_eq!(body["versions"][0]["recommended"], false);
    assert_eq!(body["versions"][1]["recommended"], true);
    let forge_all = get(&gateway.router, "/v1/minecraft/loaders/forge/versions").await;
    let body = forge_all.json();
    assert_eq!(ids(&body), ["52.0.1", "47.3.0", "47.2.0"]);
    assert_eq!(body["versions"][0]["recommended"], true);

    let fabric = get(&gateway.router, "/v1/minecraft/loaders/fabric/versions").await;
    assert_eq!(ids(&fabric.json()), ["0.16.5", "0.16.4", "0.16.0-rc.1"]);
    assert_eq!(fabric.json()["versions"][2]["stable"], false);
    let fabric_for = get(
        &gateway.router,
        "/v1/minecraft/loaders/fabric/versions?minecraft=1.21.1",
    )
    .await;
    assert_eq!(ids(&fabric_for.json()).len(), 3);
    let fabric_none = get(
        &gateway.router,
        "/v1/minecraft/loaders/fabric/versions?minecraft=1.7.10",
    )
    .await;
    assert_eq!(ids(&fabric_none.json()), Vec::<&str>::new());

    let unknown = get(&gateway.router, "/v1/minecraft/loaders/quilt/versions").await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    assert_eq!(unknown.json()["code"], "UNKNOWN_LOADER");
}

#[tokio::test]
async fn a_source_that_fails_answers_the_old_list_or_a_502() {
    let mock = Mock::start(Duration::ZERO, |call, seen| {
        if call >= 2 {
            reply(503, "")
        } else {
            sources(call, seen)
        }
    })
    .await;
    let gateway = gateway(&mock);
    get(&gateway.router, "/v1/minecraft/versions").await;
    get(&gateway.router, "/v1/minecraft/loaders/fabric/versions").await;

    gateway.clock.advance(LIST_FRESH_MS + 1);
    let stale = get(&gateway.router, "/v1/minecraft/versions").await;
    assert_eq!(stale.status, StatusCode::OK);
    assert_eq!(ids(&stale.json()), ["1.21.1", "1.21", "1.20.1"]);

    let fresh = get(&gateway.router, "/v1/minecraft/loaders/forge/versions").await;
    assert_eq!(fresh.status, StatusCode::BAD_GATEWAY);
    assert_eq!(fresh.json()["code"], "UPSTREAM_UNREADABLE");
    assert!(fresh.json()["message"].as_str().unwrap().contains("Forge"));
}

#[test]
fn maps_neoforge_builds_to_their_minecraft_version() {
    assert_eq!(neoforge_minecraft("21.1.57").as_deref(), Some("1.21.1"));
    assert_eq!(neoforge_minecraft("21.0.0-beta").as_deref(), Some("1.21"));
    assert_eq!(neoforge_minecraft("20.4.80").as_deref(), Some("1.20.4"));
    assert_eq!(neoforge_minecraft("26.1.0.12").as_deref(), Some("26.1"));
    assert_eq!(
        neoforge_minecraft("26.1.1.3-alpha").as_deref(),
        Some("26.1.1")
    );
    assert_eq!(neoforge_minecraft("26.1"), None);
    assert_eq!(neoforge_minecraft("nope"), None);
}

#[tokio::test]
async fn year_based_builds_and_alpha_builds_are_listed_right() {
    let mock = Mock::start(Duration::ZERO, |_, seen| match seen.uri.as_str() {
        NEOFORGE_PATH => reply(
            200,
            r#"{"isSnapshot":false,"versions":["21.1.72","26.1.0.1-alpha","26.1.0.12","26.1.1.3"]}"#,
        ),
        MANIFEST_PATH => reply(
            200,
            json!({
                "latest": { "release": "26.1.1", "snapshot": "26.1.1" },
                "versions": [
                    { "id": "26.1.1", "type": "release", "url": "x", "time": "x", "releaseTime": "2026-05-01T00:00:00+00:00", "sha1": "a", "complianceLevel": 1 },
                    { "id": "26.1", "type": "release", "url": "x", "time": "x", "releaseTime": "2026-03-01T00:00:00+00:00", "sha1": "b", "complianceLevel": 1 },
                    { "id": "1.21.1", "type": "release", "url": "x", "time": "x", "releaseTime": "2024-08-08T00:00:00+00:00", "sha1": "c", "complianceLevel": 1 }
                ]
            })
            .to_string(),
        ),
        uri => panic!("unexpected {uri}"),
    })
    .await;
    let gateway = gateway(&mock);

    let builds = get(
        &gateway.router,
        "/v1/minecraft/loaders/neoforge/versions?minecraft=26.1",
    )
    .await;
    let body = builds.json();
    assert_eq!(ids(&body), ["26.1.0.12", "26.1.0.1-alpha"]);
    assert_eq!(body["versions"][0]["stable"], true);
    assert_eq!(body["versions"][1]["stable"], false);

    let versions = get(&gateway.router, "/v1/minecraft/versions?loader=neoforge").await;
    assert_eq!(ids(&versions.json()), ["26.1.1", "26.1", "1.21.1"]);
}

fn runtime(base: &str, name: &str, sha1: &str) -> Value {
    json!([{
        "availability": { "group": 1, "progress": 100 },
        "manifest": { "sha1": sha1, "size": 82843, "url": format!("{base}/v1/packages/{sha1}/manifest.json") },
        "version": { "name": name, "released": "2025-05-19T08:30:12+00:00" }
    }])
}

fn runtimes(_: usize, seen: &Seen) -> Reply {
    let base = seen.base.as_str();
    assert_eq!(seen.uri, JAVA_RUNTIMES_PATH);
    reply(
        200,
        json!({
            "gamecore": { "java-runtime-delta": [], "jre-legacy": [] },
            "linux": {
                "java-runtime-alpha": runtime(base, "16.0.1.9.1", "a"),
                "java-runtime-beta": runtime(base, "17.0.15", "b"),
                "java-runtime-delta": runtime(base, "21.0.7", "d"),
                "java-runtime-gamma": runtime(base, "17.0.15", "g"),
                "java-runtime-gamma-snapshot": runtime(base, "17.0.16", "s"),
                "jre-legacy": runtime(base, "8u202", "l"),
                "java-runtime-zeta": [{ "manifest": { "size": "large" }, "version": { "name": "21.0.9" } }],
                "minecraft-java-exe": []
            },
            "windows-x64": { "java-runtime-delta": [{ "version": { "name": "21.0.7" } }] },
            "broken": 5,
            "windows-x86": {
                "java-runtime-beta": runtime(base, "17.0.1.12.1", "wb"),
                "java-runtime-gamma": runtime(base, "17.0.8.1", "wg")
            }
        })
        .to_string(),
    )
}

#[tokio::test]
async fn a_java_runtime_is_found_by_major_or_component_from_one_kept_list() {
    let mock = Mock::start(Duration::ZERO, runtimes).await;
    let gateway = gateway(&mock);

    let got = get(
        &gateway.router,
        "/v1/minecraft/java?platform=linux&major=21",
    )
    .await;
    assert_eq!(got.status, StatusCode::OK);
    assert_eq!(
        got.json(),
        json!({
            "platform": "linux",
            "component": "java-runtime-delta",
            "major": 21,
            "version": "21.0.7",
            "released": "2025-05-19T08:30:12+00:00",
            "manifest": { "url": format!("{}/v1/packages/d/manifest.json", mock.base), "sha1": "d", "size": 82843 }
        })
    );

    let legacy = get(&gateway.router, "/v1/minecraft/java?platform=linux&major=8").await;
    assert_eq!(legacy.json()["component"], "jre-legacy");
    assert_eq!(legacy.json()["version"], "8u202");
    let seventeen = get(
        &gateway.router,
        "/v1/minecraft/java?platform=linux&major=17",
    )
    .await;
    assert_eq!(seventeen.json()["component"], "java-runtime-gamma");
    let older = get(
        &gateway.router,
        "/v1/minecraft/java?platform=windows-x86&major=17",
    )
    .await;
    assert_eq!(older.json()["component"], "java-runtime-gamma");
    assert_eq!(older.json()["version"], "17.0.8.1");

    let named = get(
        &gateway.router,
        "/v1/minecraft/java?platform=linux&component=java-runtime-gamma-snapshot",
    )
    .await;
    assert_eq!(named.json()["version"], "17.0.16");
    assert_eq!(named.json()["major"], 17);
    assert_eq!(mock.calls(), 1);

    gateway.clock.advance(LIST_FRESH_MS + 1);
    get(
        &gateway.router,
        "/v1/minecraft/java?platform=linux&major=21",
    )
    .await;
    assert_eq!(mock.calls(), 2);
    assert_eq!(gateway.state.curseforge_budget.snapshot().await.days, []);
}

#[tokio::test]
async fn a_java_runtime_mojang_does_not_ship_or_a_bad_query_is_refused() {
    let mock = Mock::start(Duration::ZERO, runtimes).await;
    let gateway = gateway(&mock);

    for missing in [
        "/v1/minecraft/java?platform=linux&major=25",
        "/v1/minecraft/java?platform=gamecore&major=21",
        "/v1/minecraft/java?platform=solaris&major=21",
        "/v1/minecraft/java?platform=linux&component=minecraft-java-exe",
        "/v1/minecraft/java?platform=linux&component=java-runtime-zeta",
        "/v1/minecraft/java?platform=windows-x64&major=21",
        "/v1/minecraft/java?platform=broken&major=21",
    ] {
        let got = get(&gateway.router, missing).await;
        assert_eq!(got.status, StatusCode::NOT_FOUND, "{missing}");
        assert_eq!(got.json()["code"], "UNKNOWN_RUNTIME");
    }

    for bad in [
        "/v1/minecraft/java?major=21",
        "/v1/minecraft/java?platform=linux",
        "/v1/minecraft/java?platform=linux&major=21&component=jre-legacy",
        "/v1/minecraft/java?platform=linux&major=new",
    ] {
        let got = get(&gateway.router, bad).await;
        assert_eq!(got.status, StatusCode::BAD_REQUEST, "{bad}");
        assert_eq!(got.json()["code"], "INVALID_QUERY");
    }
}

#[tokio::test]
async fn a_java_runtime_manifest_is_taken_from_mojang_s_host_only() {
    let mock = Mock::start(Duration::ZERO, |_, seen| {
        let elsewhere = format!("{}.example.net", seen.base);
        reply(
            200,
            json!({ "linux": { "java-runtime-delta": runtime(&elsewhere, "21.0.7", "d") } })
                .to_string(),
        )
    })
    .await;
    let gateway = gateway(&mock);
    let got = get(
        &gateway.router,
        "/v1/minecraft/java?platform=linux&major=21",
    )
    .await;
    assert_eq!(got.status, StatusCode::BAD_GATEWAY);
}

#[test]
fn equal_runtime_versions_go_to_the_later_release_before_the_name() {
    let listing = |sha1: &str, released: &str| {
        json!([{
            "manifest": { "sha1": sha1, "size": 1, "url": "x" },
            "version": { "name": "21.0.7", "released": released }
        }])
    };
    let index: RuntimeIndex = serde_json::from_value(json!({
        "linux": {
            "java-runtime-delta": listing("d", "2025-06-01T00:00:00+00:00"),
            "java-runtime-gamma": listing("g", "2025-05-01T00:00:00+00:00")
        }
    }))
    .unwrap();
    let picked = pick(&index, "linux", &Wanted::Major(21)).unwrap();
    assert_eq!(picked.component, "java-runtime-delta");
}

#[test]
fn reads_the_java_major_of_mojang_s_runtime_names() {
    assert_eq!(runtime_major("21.0.7"), Some(21));
    assert_eq!(runtime_major("8u202"), Some(8));
    assert_eq!(runtime_major("16.0.1.9.1_3"), Some(16));
    assert_eq!(runtime_major("jre"), None);
}

#[tokio::test]
async fn a_version_document_is_read_from_mojang_s_host_only() {
    let mock = Mock::start(Duration::ZERO, |_, seen| match seen.uri.as_str() {
        MANIFEST_PATH => {
            let elsewhere = format!("{}.example.net/v1/packages/x.json", seen.base);
            reply(
                200,
                json!({
                    "latest": { "release": "1.21.1", "snapshot": "1.21.1" },
                    "versions": [{ "id": "1.21.1", "type": "release", "url": elsewhere, "time": "x", "releaseTime": "2024-08-08T00:00:00+00:00", "sha1": "a", "complianceLevel": 1 }]
                })
                .to_string(),
            )
        }
        uri => panic!("unexpected {uri}"),
    })
    .await;
    let gateway = gateway(&mock);
    let got = get(&gateway.router, "/v1/minecraft/versions/1.21.1").await;
    assert_eq!(got.status, StatusCode::BAD_GATEWAY);
    assert_eq!(mock.calls(), 1);
}

#[tokio::test]
async fn a_200_the_gateway_cannot_read_is_a_502_and_not_kept() {
    let mock = Mock::start(Duration::ZERO, |_, _| reply(200, "not json")).await;
    let gateway = gateway(&mock);
    for _ in 0..2 {
        let got = get(&gateway.router, "/v1/minecraft/versions").await;
        assert_eq!(got.status, StatusCode::BAD_GATEWAY);
        assert_eq!(got.json()["code"], "UPSTREAM_UNREADABLE");
        assert!(got.json()["message"].as_str().unwrap().contains("Mojang"));
    }
    assert_eq!(mock.calls(), 2);
}

#[tokio::test]
async fn an_unreadable_200_after_a_good_list_answers_the_old_list() {
    let mock = Mock::start(Duration::ZERO, |call, seen| {
        if call == 0 {
            sources(call, seen)
        } else {
            reply(200, r#"{"versions":"not a list"}"#)
        }
    })
    .await;
    let gateway = gateway(&mock);
    let fresh = get(&gateway.router, "/v1/minecraft/loaders/neoforge/versions").await;

    gateway.clock.advance(LIST_FRESH_MS + 1);
    for _ in 0..2 {
        let stale = get(&gateway.router, "/v1/minecraft/loaders/neoforge/versions").await;
        assert_eq!(stale.status, StatusCode::OK);
        assert_eq!(stale.json(), fresh.json());
    }
    assert_eq!(mock.calls(), 3);
}

#[tokio::test]
async fn a_kept_copy_that_does_not_read_counts_as_no_copy() {
    let (mock, _never) = Mock::held_when(|seen| seen.uri == NEOFORGE_PATH, sources).await;
    let gateway = gateway_with(
        &mock,
        GatewayWith {
            timeouts: Timeouts {
                meta: Duration::from_millis(50),
                ..Timeouts::default()
            },
            ..GatewayWith::default()
        },
    );
    let unreadable = |fresh_ms| {
        Arc::new(Entry::new(
            Answer {
                status: StatusCode::OK,
                content_type: None,
                body: Bytes::from_static(b"not json"),
            },
            NOV_3_2026_20H,
            fresh_ms,
        ))
    };
    gateway
        .state
        .cache
        .insert("loader:forge", unreadable(LIST_FRESH_MS));
    let fresh = get(&gateway.router, "/v1/minecraft/loaders/forge/versions").await;
    assert_eq!(fresh.status, StatusCode::OK);
    assert_eq!(mock.calls(), 1);

    gateway.state.cache.insert("loader:neoforge", unreadable(0));
    let failed = get(&gateway.router, "/v1/minecraft/loaders/neoforge/versions").await;
    assert_eq!(failed.status, StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(failed.json()["code"], "UPSTREAM_TIMEOUT");
    assert_eq!(mock.calls(), 2);
}

#[tokio::test]
async fn a_loader_query_that_does_not_read_is_refused() {
    let mock = Mock::start(Duration::ZERO, sources).await;
    let gateway = gateway(&mock);
    let got = get(
        &gateway.router,
        "/v1/minecraft/loaders/forge/versions?minecraft=1.20.1&minecraft=1.21.1",
    )
    .await;
    assert_eq!(got.status, StatusCode::BAD_REQUEST);
    assert_eq!(got.json()["code"], "INVALID_QUERY");
    assert_eq!(mock.calls(), 0);
}

#[tokio::test]
async fn a_source_that_does_not_answer_in_time_is_a_504() {
    let (mock, _never) = Mock::held(sources).await;
    let gateway = gateway_with(
        &mock,
        GatewayWith {
            timeouts: Timeouts {
                meta: Duration::from_millis(50),
                ..Timeouts::default()
            },
            ..GatewayWith::default()
        },
    );
    let got = get(&gateway.router, "/v1/minecraft/loaders/neoforge/versions").await;
    assert_eq!(got.status, StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(got.json()["code"], "UPSTREAM_TIMEOUT");
}

#[tokio::test]
async fn requests_for_a_list_already_being_fetched_wait_for_it() {
    let (mock, gate) = Mock::held(sources).await;
    let gateway = gateway(&mock);
    let first = {
        let router = gateway.router.clone();
        tokio::spawn(async move { get(&router, "/v1/minecraft/versions").await })
    };
    until("the manifest call to reach Mojang", || mock.calls() == 1).await;
    let second = {
        let router = gateway.router.clone();
        tokio::spawn(async move { get(&router, "/v1/minecraft/versions?type=all").await })
    };
    until("the second request joining it", || {
        gateway.state.cache.joined() == 1
    })
    .await;
    gate.open();

    assert_eq!(first.await.unwrap().status, StatusCode::OK);
    let second = second.await.unwrap();
    assert_eq!(second.status, StatusCode::OK);
    assert_eq!(ids(&second.json()).len(), 5);
    assert_eq!(mock.calls(), 1);
}

#[tokio::test]
async fn a_loader_list_is_answered_old_when_its_source_fails() {
    let mock = Mock::start(Duration::ZERO, |call, seen| {
        if call >= 1 {
            reply(503, "")
        } else {
            sources(call, seen)
        }
    })
    .await;
    let gateway = gateway(&mock);
    let fresh = get(&gateway.router, "/v1/minecraft/loaders/neoforge/versions").await;

    gateway.clock.advance(LIST_FRESH_MS + 1);
    let stale = get(&gateway.router, "/v1/minecraft/loaders/neoforge/versions").await;
    assert_eq!(stale.status, StatusCode::OK);
    assert_eq!(stale.json(), fresh.json());
    assert_eq!(mock.calls(), 2);
}

#[tokio::test]
async fn an_empty_minecraft_version_lists_every_build() {
    let mock = Mock::start(Duration::ZERO, sources).await;
    let gateway = gateway(&mock);
    let every = get(&gateway.router, "/v1/minecraft/loaders/forge/versions").await;
    let empty = get(
        &gateway.router,
        "/v1/minecraft/loaders/forge/versions?minecraft=",
    )
    .await;
    assert_eq!(empty.status, StatusCode::OK);
    assert_eq!(empty.json()["minecraft"], Value::Null);
    assert_eq!(empty.json(), every.json());
}
