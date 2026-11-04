use std::sync::Arc;

use axum::{Router, middleware, routing::get};

use crate::{
    auth,
    budget::{self, Budget, Clock, SystemClock},
    cache::AnswerCache,
    config::{Config, Consumer},
    curseforge::{self, CURSEFORGE_API, Upstream},
    health, openapi,
    redis_store::RedisStore,
};

pub struct AppState {
    pub consumers: Vec<Consumer>,
    pub budget: Budget,
    pub upstream: Upstream,
    pub cache: AnswerCache,
    pub clock: Arc<dyn Clock>,
}

impl AppState {
    pub fn from_config(config: &Config, redis: Option<Arc<RedisStore>>) -> reqwest::Result<Self> {
        let pools = config.consumers.iter().flat_map(|consumer| {
            consumer
                .pools
                .iter()
                .map(|pool| (consumer.name.as_str(), pool.as_str()))
        });
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        Ok(Self {
            budget: Budget::new(
                config.budget_per_minute,
                pools,
                clock.clone(),
                redis.clone(),
            ),
            upstream: Upstream::new(
                CURSEFORGE_API,
                config.curseforge_api_key.clone(),
                curseforge::TIMEOUT,
            )?,
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
        budget: Budget::new(300, pools, Arc::new(SystemClock), None),
        upstream: Upstream::new(
            "http://127.0.0.1:9",
            crate::config::ApiKey::new("unused"),
            curseforge::TIMEOUT,
        )
        .unwrap(),
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
        assert_eq!(body["limitPerMinute"], 300);
        assert_eq!(body["used"], 0);
        assert_eq!(body["heldUntil"], Value::Null);
        assert_eq!(body["source"], "memory");
        assert_eq!(body["pools"][0]["consumer"], "app");
        assert_eq!(body["pools"][1]["pool"], "app");
    }

    #[tokio::test]
    async fn unknown_routes_answer_not_found_without_a_token() {
        let (status, _, _) = get("/v1/nothing-here", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
