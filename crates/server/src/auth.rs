use std::sync::Arc;

use axum::{
    extract::{FromRequestParts, Request, State},
    http::{HeaderMap, header, request::Parts},
    middleware::Next,
    response::{IntoResponse, Response},
};
use subtle::ConstantTimeEq;

use crate::{app::AppState, config::Consumer, error::ApiError};

const POOL_HEADER: &str = "x-budget-pool";

#[derive(Debug, Clone)]
struct Caller {
    consumer: Arc<str>,
    pools: Arc<[Arc<str>]>,
}

impl From<&Consumer> for Caller {
    fn from(consumer: &Consumer) -> Self {
        Self {
            consumer: consumer.name.as_str().into(),
            pools: consumer
                .pools
                .iter()
                .map(|pool| Arc::from(pool.as_str()))
                .collect(),
        }
    }
}

/// The budget pool a request draws from: the `X-Budget-Pool` header, or the token's only pool when it has one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pool {
    pub consumer: Arc<str>,
    pub name: Arc<str>,
}

pub async fn authenticate(
    State(state): State<Arc<AppState>>,
    mut request: Request,
    next: Next,
) -> Response {
    let Some(caller) = bearer(request.headers())
        .and_then(|token| find_consumer(&state.consumers, token))
        .map(Caller::from)
    else {
        return ApiError::unauthorized().into_response();
    };
    request.extensions_mut().insert(caller);
    next.run(request).await
}

impl<S: Send + Sync> FromRequestParts<S> for Pool {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let caller = parts
            .extensions
            .get::<Caller>()
            .cloned()
            .ok_or_else(ApiError::unauthorized)?;
        let named = parts.headers.get(POOL_HEADER);
        let pool = match named {
            None => match caller.pools.as_ref() {
                [only] => only.clone(),
                pools => return Err(ApiError::pool_required(pools)),
            },
            Some(value) => {
                let requested = value.to_str().unwrap_or_default();
                caller
                    .pools
                    .iter()
                    .find(|pool| pool.as_ref() == requested)
                    .cloned()
                    .ok_or_else(|| ApiError::unknown_pool(requested, &caller.pools))?
            }
        };
        Ok(Self {
            consumer: caller.consumer,
            name: pool,
        })
    }
}

fn bearer(headers: &HeaderMap) -> Option<&[u8]> {
    let value = headers.get(header::AUTHORIZATION)?.as_bytes();
    let (scheme, token) = value.split_at_checked(7)?;
    (scheme.eq_ignore_ascii_case(b"bearer ") && !token.is_empty()).then_some(token)
}

fn find_consumer<'a>(consumers: &'a [Consumer], token: &[u8]) -> Option<&'a Consumer> {
    let mut found = None;
    for consumer in consumers {
        if bool::from(consumer.token.as_bytes().ct_eq(token)) {
            found = Some(consumer);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use axum::{
        Router,
        body::Body,
        http::{Request, StatusCode},
        middleware,
        routing::get,
    };
    use serde_json::Value;

    use super::*;
    use crate::test_support::{TOKEN as APP_TOKEN, call as call_router, consumer, test_state};

    const SANDBOX_TOKEN: &str = "sandbox-token-0123456789abcdef012345";

    fn router() -> Router {
        let state = test_state(vec![
            consumer("app", APP_TOKEN, &["launcher", "app"]),
            consumer("sandbox", SANDBOX_TOKEN, &["jobs"]),
        ]);
        Router::new()
            .route(
                "/pool",
                get(|pool: Pool| async move { format!("{}/{}", pool.consumer, pool.name) }),
            )
            .route_layer(middleware::from_fn_with_state(state.clone(), authenticate))
            .with_state(state)
    }

    async fn call(headers: &[(&str, &str)]) -> (StatusCode, String) {
        let mut request = Request::get("/pool");
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let got = call_router(&router(), request.body(Body::empty()).unwrap()).await;
        (got.status, got.text())
    }

    fn code(body: &str) -> String {
        let body: Value = serde_json::from_str(body).unwrap();
        body["code"].as_str().unwrap().to_owned()
    }

    #[tokio::test]
    async fn refuses_requests_without_a_known_token() {
        for headers in [
            vec![],
            vec![("authorization", "Bearer wrong-token-0123456789abcdef0123")],
            vec![("authorization", APP_TOKEN)],
            vec![("authorization", "Basic YXBwOnRva2Vu")],
            vec![("authorization", "Bearer ")],
        ] {
            let (status, body) = call(&headers).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{headers:?}");
            assert_eq!(code(&body), "UNAUTHORIZED");
        }
    }

    #[tokio::test]
    async fn a_token_with_one_pool_needs_no_pool_header() {
        let bearer = format!("Bearer {SANDBOX_TOKEN}");
        assert_eq!(
            call(&[("authorization", &bearer)]).await,
            (StatusCode::OK, "sandbox/jobs".to_owned())
        );
    }

    #[tokio::test]
    async fn the_scheme_is_case_insensitive() {
        let bearer = format!("bearer {SANDBOX_TOKEN}");
        assert_eq!(call(&[("authorization", &bearer)]).await.0, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_token_with_several_pools_must_name_one() {
        let bearer = format!("Bearer {APP_TOKEN}");
        let (status, body) = call(&[("authorization", &bearer)]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(code(&body), "POOL_REQUIRED");

        assert_eq!(
            call(&[("authorization", &bearer), ("x-budget-pool", "launcher")]).await,
            (StatusCode::OK, "app/launcher".to_owned())
        );
    }

    #[tokio::test]
    async fn a_pool_of_another_consumer_is_refused() {
        let bearer = format!("Bearer {APP_TOKEN}");
        let (status, body) = call(&[("authorization", &bearer), ("x-budget-pool", "jobs")]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(code(&body), "UNKNOWN_POOL");
    }
}
