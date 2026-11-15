use std::{sync::Arc, time::Duration};

use axum::{
    Router, middleware,
    routing::{get, post},
};

use crate::{
    auth,
    budget::{self, Budget, Clock},
    cache::AnswerCache,
    config::{Config, Consumer},
    curseforge::{self, CURSEFORGE_API},
    health,
    modrinth::{self, MODRINTH_API},
    openapi,
    redis_store::RedisStore,
    resolve::{self, CURSEFORGE_CDN, Cdn},
    upstream::{Source, Upstream},
    versions::{self, FABRIC_META, FORGE_FILES, MOJANG_META, Meta, NEOFORGE_MAVEN},
};

pub struct AppState {
    pub consumers: Vec<Consumer>,
    /// The budget of calls on the CurseForge key.
    pub budget: Budget,
    /// The budget of calls to Modrinth.
    pub modrinth_budget: Budget,
    pub upstream: Upstream,
    pub modrinth: Upstream,
    pub cdn: Cdn,
    pub meta: Meta,
    pub cache: AnswerCache,
    pub clock: Arc<dyn Clock>,
}

impl AppState {
    pub fn upstream_of(&self, source: Source) -> &Upstream {
        match source {
            Source::CurseForge => &self.upstream,
            Source::Modrinth => &self.modrinth,
        }
    }

    pub fn budget_of(&self, source: Source) -> &Budget {
        match source {
            Source::CurseForge => &self.budget,
            Source::Modrinth => &self.modrinth_budget,
        }
    }

    pub fn new(
        config: &Config,
        addresses: &Addresses,
        timeouts: Timeouts,
        clock: Arc<dyn Clock>,
        redis: Option<Arc<RedisStore>>,
    ) -> reqwest::Result<Self> {
        let pools = config.consumers.iter().flat_map(|consumer| {
            consumer
                .pools
                .iter()
                .map(|pool| (consumer.name.as_str(), pool.as_str()))
        });
        let pools: Vec<(&str, &str)> = pools.collect();
        Ok(Self {
            budget: Budget::new(
                Source::CurseForge,
                config.budget_per_minute,
                pools.iter().copied(),
                clock.clone(),
                redis.clone(),
            )
            .keeping_days(config.budget_history_days),
            modrinth_budget: Budget::new(
                Source::Modrinth,
                config.modrinth_per_minute,
                pools.iter().copied(),
                clock.clone(),
                redis.clone(),
            )
            .keeping_days(config.budget_history_days),
            upstream: Upstream::new(
                Source::CurseForge,
                &addresses.curseforge_api,
                Some(config.curseforge_api_key.clone()),
                &config.user_agent,
                timeouts.curseforge,
            )?,
            modrinth: Upstream::new(
                Source::Modrinth,
                &addresses.modrinth_api,
                None,
                &config.user_agent,
                timeouts.modrinth,
            )?,
            cdn: Cdn::new(&addresses.curseforge_cdn, &config.user_agent, timeouts.cdn)?,
            meta: Meta::new(
                &addresses.mojang_meta,
                &addresses.neoforge_maven,
                &addresses.forge_files,
                &addresses.fabric_meta,
                &config.user_agent,
                timeouts.meta,
            )?,
            cache: AnswerCache::new(redis),
            clock,
            consumers: config.consumers.clone(),
        })
    }
}

/// Where each upstream client sends its calls.
pub struct Addresses {
    pub curseforge_api: String,
    pub modrinth_api: String,
    pub curseforge_cdn: String,
    pub mojang_meta: String,
    pub neoforge_maven: String,
    pub forge_files: String,
    pub fabric_meta: String,
}

impl Default for Addresses {
    fn default() -> Self {
        Self {
            curseforge_api: CURSEFORGE_API.to_owned(),
            modrinth_api: MODRINTH_API.to_owned(),
            curseforge_cdn: CURSEFORGE_CDN.to_owned(),
            mojang_meta: MOJANG_META.to_owned(),
            neoforge_maven: NEOFORGE_MAVEN.to_owned(),
            forge_files: FORGE_FILES.to_owned(),
            fabric_meta: FABRIC_META.to_owned(),
        }
    }
}

/// How long each upstream client waits for an answer.
#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    pub curseforge: Duration,
    pub modrinth: Duration,
    pub cdn: Duration,
    pub meta: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            curseforge: curseforge::TIMEOUT,
            modrinth: modrinth::TIMEOUT,
            cdn: resolve::CDN_TIMEOUT,
            meta: versions::TIMEOUT,
        }
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    let authenticated = Router::new()
        .route("/v1/budget", get(budget::readout))
        .route(
            "/v1/curseforge/{*path}",
            get(curseforge::passthrough_get).post(curseforge::passthrough_post),
        )
        .route(
            "/v1/modrinth/{*path}",
            get(modrinth::passthrough_get).post(modrinth::passthrough_post),
        )
        .route("/v1/resolve", post(resolve::resolve))
        .route("/v1/minecraft/versions", get(versions::minecraft_versions))
        .route(
            "/v1/minecraft/versions/{id}",
            get(versions::minecraft_version),
        )
        .route(
            "/v1/minecraft/loaders/{loader}/versions",
            get(versions::loader_versions),
        )
        .route("/v1/minecraft/java", get(versions::java_runtime))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::authenticate,
        ));

    Router::new()
        .route("/v1/healthz", get(health::healthz))
        .route("/v1/openapi.json", get(openapi::document))
        .merge(authenticated)
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use serde_json::Value;

    use super::*;
    use crate::test_support::{
        self, KEY, Mock, NOV_3_2026_20H, TOKEN, TestClock, USER_AGENT, addresses_at, call,
        consumer, reply, test_state,
    };

    fn app() -> Router {
        router(test_state(vec![consumer(
            "app",
            TOKEN,
            &["launcher", "app"],
        )]))
    }

    async fn get(uri: &str, token: Option<&str>) -> (StatusCode, Option<String>, Vec<u8>) {
        let got = match token {
            Some(TOKEN) => test_support::get(&app(), uri).await,
            Some(other) => panic!("no consumer has the token {other}"),
            None => call(&app(), Request::get(uri).body(Body::empty()).unwrap()).await,
        };
        let content_type = got.header("content-type").map(str::to_owned);
        (got.status, content_type, got.body.to_vec())
    }

    #[tokio::test]
    async fn healthz_answers_ok_as_json_without_a_token() {
        let (status, content_type, body) = get("/v1/healthz", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type.as_deref(), Some("application/json"));

        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["status"], "ok");
        assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
        assert!(body.get("commit").is_some());
        assert!(body.get("builtAt").is_some());
    }

    #[tokio::test]
    async fn the_openapi_document_is_served_without_a_token_as_checked_in() {
        let (status, content_type, body) = get("/v1/openapi.json", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type.as_deref(), Some("application/json"));

        let served: Value = serde_json::from_slice(&body).unwrap();
        let checked_in: Value =
            serde_json::from_str(include_str!("../../../openapi.json")).unwrap();
        assert_eq!(served, checked_in);
        assert_eq!(served["info"]["version"], openapi::API_VERSION);
    }

    #[tokio::test]
    async fn the_budget_needs_a_token() {
        let (status, _, _) = get("/v1/budget", None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn the_budget_lists_every_pool_without_a_pool_header() {
        let (status, _, body) = get("/v1/budget", Some(TOKEN)).await;
        assert_eq!(status, StatusCode::OK);

        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["source"], "memory");
        assert_eq!(body["historyDays"], 90);
        for upstream in ["curseforge", "modrinth"] {
            let budget = &body[upstream];
            assert_eq!(budget["limitPerMinute"], 300, "{upstream}");
            assert_eq!(budget["used"], 0, "{upstream}");
            assert_eq!(budget["heldUntil"], Value::Null, "{upstream}");
            assert_eq!(budget["pools"][0]["consumer"], "app", "{upstream}");
            assert_eq!(budget["pools"][1]["pool"], "app", "{upstream}");
            assert_eq!(budget.get("source"), None, "{upstream}");
        }
    }

    #[tokio::test]
    async fn the_budget_answers_the_days_asked_for_and_refuses_a_bad_number() {
        let (status, _, body) = get("/v1/budget?days=30", Some(TOKEN)).await;
        assert_eq!(status, StatusCode::OK);
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["historyDays"], 90);
        assert_eq!(body["curseforge"]["days"], Value::Array(Vec::new()));

        for query in ["days=0", "days=-1", "days=many", "days="] {
            let (status, _, body) = get(&format!("/v1/budget?{query}"), Some(TOKEN)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{query}");
            let body: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["code"], "INVALID_DAYS", "{query}");
        }
    }

    #[tokio::test]
    async fn the_config_gives_each_client_its_key_and_user_agent_and_sets_the_budgets() {
        let mock = Mock::start(Duration::ZERO, |_, _| reply(200, "{}")).await;
        let config = Config::from_lookup(|name| {
            let value = match name {
                "CURSEFORGE_API_KEY" => KEY,
                "CONTENT_GATEWAY_TOKEN" => TOKEN,
                "CONTENT_GATEWAY_USER_AGENT" => USER_AGENT,
                "CONTENT_GATEWAY_BUDGET_PER_MINUTE" => "7",
                "CONTENT_GATEWAY_MODRINTH_PER_MINUTE" => "8",
                "CONTENT_GATEWAY_BUDGET_HISTORY_DAYS" => "30",
                _ => return None,
            };
            Some(value.to_owned())
        })
        .unwrap();
        let state = AppState::new(
            &config,
            &addresses_at(&mock.base),
            Timeouts::default(),
            TestClock::at(NOV_3_2026_20H),
            None,
        )
        .unwrap();
        let app = router(Arc::new(state));

        for uri in [
            "/v1/curseforge/mods/1",
            "/v1/modrinth/project/jei",
            "/v1/minecraft/versions",
        ] {
            test_support::get(&app, uri).await;
        }
        let seen: Vec<(String, Option<String>, Option<String>)> = mock
            .seen()
            .into_iter()
            .map(|seen| (seen.uri, seen.key, seen.user_agent))
            .collect();
        let user_agent = Some(USER_AGENT.to_owned());
        assert_eq!(
            seen,
            [
                (
                    "/v1/mods/1".to_owned(),
                    Some(KEY.to_owned()),
                    user_agent.clone()
                ),
                ("/v2/project/jei".to_owned(), None, user_agent.clone()),
                (
                    "/mc/game/version_manifest_v2.json".to_owned(),
                    None,
                    user_agent
                ),
            ]
        );

        let budget = test_support::get(&app, "/v1/budget").await.json();
        assert_eq!(budget["historyDays"], 30);
        assert_eq!(budget["curseforge"]["limitPerMinute"], 7);
        assert_eq!(budget["modrinth"]["limitPerMinute"], 8);
        assert_eq!(budget["curseforge"]["pools"][0]["consumer"], "default");
    }

    #[tokio::test]
    async fn unknown_routes_answer_not_found_without_a_token() {
        let (status, _, _) = get("/v1/nothing-here", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
