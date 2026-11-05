use std::sync::Arc;

use axum::{
    Router, middleware,
    routing::{get, post},
};

use crate::{
    auth,
    budget::{self, Budget, Clock, SystemClock},
    cache::AnswerCache,
    config::{Config, Consumer},
    curseforge::{self, CURSEFORGE_API},
    health,
    modrinth::{self, MODRINTH_API},
    openapi,
    redis_store::RedisStore,
    resolve::{self, CURSEFORGE_CDN, Cdn},
    upstream::{Source, Upstream},
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

    pub fn from_config(config: &Config, redis: Option<Arc<RedisStore>>) -> reqwest::Result<Self> {
        let pools = config.consumers.iter().flat_map(|consumer| {
            consumer
                .pools
                .iter()
                .map(|pool| (consumer.name.as_str(), pool.as_str()))
        });
        let pools: Vec<(&str, &str)> = pools.collect();
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        Ok(Self {
            budget: Budget::new(
                config.budget_per_minute,
                pools.iter().copied(),
                clock.clone(),
                redis.clone(),
            )
            .keeping_days(config.budget_history_days),
            modrinth_budget: Budget::new(
                config.modrinth_per_minute,
                pools.iter().copied(),
                clock.clone(),
                redis.clone(),
            )
            .keeping_days(config.budget_history_days)
            .named("modrinth", "Modrinth"),
            upstream: Upstream::new(
                Source::CurseForge,
                CURSEFORGE_API,
                Some(config.curseforge_api_key.clone()),
                curseforge::USER_AGENT,
                curseforge::TIMEOUT,
            )?,
            modrinth: Upstream::new(
                Source::Modrinth,
                MODRINTH_API,
                None,
                &config.modrinth_user_agent,
                modrinth::TIMEOUT,
            )?,
            cdn: Cdn::new(CURSEFORGE_CDN, resolve::CDN_TIMEOUT)?,
            cache: AnswerCache::new(redis),
            clock,
            consumers: config.consumers.clone(),
        })
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
pub(crate) fn test_state(consumers: Vec<Consumer>) -> Arc<AppState> {
    let pools: Vec<(String, String)> = consumers
        .iter()
        .flat_map(|consumer| {
            consumer
                .pools
                .iter()
                .map(|pool| (consumer.name.clone(), pool.clone()))
        })
        .collect();
    Arc::new(AppState {
        budget: Budget::new(300, pools.clone(), Arc::new(SystemClock), None),
        modrinth_budget: Budget::new(300, pools, Arc::new(SystemClock), None),
        upstream: Upstream::new(
            Source::CurseForge,
            "http://127.0.0.1:9/v1",
            Some(crate::config::ApiKey::new("unused")),
            curseforge::USER_AGENT,
            curseforge::TIMEOUT,
        )
        .unwrap(),
        modrinth: Upstream::new(
            Source::Modrinth,
            "http://127.0.0.1:9/v2",
            None,
            modrinth::DEFAULT_USER_AGENT,
            modrinth::TIMEOUT,
        )
        .unwrap(),
        cdn: Cdn::new("http://127.0.0.1:9", resolve::CDN_TIMEOUT).unwrap(),
        cache: AnswerCache::default(),
        clock: Arc::new(SystemClock),
        consumers,
    })
}

#[cfg(test)]
mod tests {
    use axum::{
        body::Body,
        http::{Request, StatusCode, header},
    };
    use http_body_util::BodyExt;
    use serde_json::Value;
    use tower::ServiceExt;

    use super::*;
    use crate::config::Token;

    const TOKEN: &str = "app-token-0123456789abcdef0123456789";

    fn app() -> Router {
        router(test_state(vec![Consumer {
            name: "app".to_owned(),
            token: Token::new(TOKEN),
            pools: vec!["launcher".to_owned(), "app".to_owned()],
        }]))
    }

    async fn get(uri: &str, token: Option<&str>) -> (StatusCode, Option<String>, Vec<u8>) {
        let mut request = Request::get(uri);
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let response = app()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .map(|value| value.to_str().unwrap().to_owned());
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, content_type, body.to_vec())
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
    async fn unknown_routes_answer_not_found_without_a_token() {
        let (status, _, _) = get("/v1/nothing-here", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
