use axum::{
    Json,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Serialize;

use crate::{budget::Refusal, curseforge::UpstreamFailure};

/// The JSON error every route answers with: `{ code, message }`, plus `pool` and `retryAfterMs` on a refused call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiError {
    #[serde(skip)]
    status: StatusCode,
    code: &'static str,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    retry_after_ms: Option<u64>,
}

impl ApiError {
    pub fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "UNAUTHORIZED",
            "send a consumer token as Authorization: Bearer <token>",
        )
    }

    pub fn pool_required(pools: &[impl AsRef<str>]) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "POOL_REQUIRED",
            format!(
                "this token draws from several pools, name one in X-Budget-Pool: {}",
                join(pools)
            ),
        )
    }

    pub fn unknown_pool(pool: &str, pools: &[impl AsRef<str>]) -> Self {
        Self {
            pool: Some(pool.to_owned()),
            ..Self::new(
                StatusCode::BAD_REQUEST,
                "UNKNOWN_POOL",
                format!(
                    "this token cannot draw from pool {pool:?}, only from: {}",
                    join(pools)
                ),
            )
        }
    }

    pub fn invalid_path() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "INVALID_PATH",
            "a CurseForge path is segments of letters, digits, - and _, separated by single slashes",
        )
    }

    pub fn upstream(failure: UpstreamFailure) -> Self {
        match failure {
            UpstreamFailure::Timeout => Self::new(
                StatusCode::GATEWAY_TIMEOUT,
                "UPSTREAM_TIMEOUT",
                "CurseForge did not answer in time",
            ),
            UpstreamFailure::Unreachable => Self::new(
                StatusCode::BAD_GATEWAY,
                "UPSTREAM_UNREACHABLE",
                "CurseForge could not be reached",
            ),
            UpstreamFailure::TooLarge => Self::new(
                StatusCode::BAD_GATEWAY,
                "UPSTREAM_TOO_LARGE",
                "CurseForge answered with more than the gateway keeps",
            ),
            UpstreamFailure::Unreadable => Self::new(
                StatusCode::BAD_GATEWAY,
                "UPSTREAM_UNREADABLE",
                "CurseForge answered a batch with something other than a list of rows with ids",
            ),
        }
    }

    pub fn refused(refusal: Refusal, pool: &str) -> Self {
        let (error, retry_after_ms) = match refusal {
            Refusal::Spent { retry_after_ms } => (
                Self::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "BUDGET_SPENT",
                    "the gateway has asked CurseForge enough this minute",
                ),
                Some(retry_after_ms),
            ),
            Refusal::Hold { retry_after_ms } => (
                Self::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "UPSTREAM_HOLD",
                    "CurseForge answered 429, every call waits until the hold ends",
                ),
                Some(retry_after_ms),
            ),
            Refusal::TooLarge {
                calls,
                limit_per_minute,
            } => (
                Self::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "REQUEST_TOO_LARGE",
                    format!(
                        "this request needs {calls} calls to CurseForge, more than the {limit_per_minute} a minute allows; split it"
                    ),
                ),
                None,
            ),
        };
        Self {
            pool: Some(pool.to_owned()),
            retry_after_ms,
            ..error
        }
    }

    pub fn status(&self) -> StatusCode {
        self.status
    }

    pub fn code(&self) -> &'static str {
        self.code
    }

    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            pool: None,
            retry_after_ms: None,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status;
        let retry_after_ms = self.retry_after_ms;
        let mut response = (status, Json(self)).into_response();
        let headers = response.headers_mut();
        if status == StatusCode::UNAUTHORIZED {
            headers.insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        if let Some(ms) = retry_after_ms {
            headers.insert(
                header::RETRY_AFTER,
                HeaderValue::from(ms.div_ceil(1000).max(1)),
            );
        }
        response
    }
}

fn join(pools: &[impl AsRef<str>]) -> String {
    pools
        .iter()
        .map(AsRef::as_ref)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use http_body_util::BodyExt;
    use serde_json::{Value, json};

    use super::*;

    async fn parts(error: ApiError) -> (StatusCode, axum::http::HeaderMap, Value) {
        let response = error.into_response();
        let status = response.status();
        let headers = response.headers().clone();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, headers, serde_json::from_slice(&body).unwrap())
    }

    #[tokio::test]
    async fn unauthorized_asks_for_a_bearer_token() {
        let (status, headers, body) = parts(ApiError::unauthorized()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(headers[header::WWW_AUTHENTICATE], "Bearer");
        assert_eq!(body["code"], "UNAUTHORIZED");
        assert!(body.get("pool").is_none());
    }

    #[tokio::test]
    async fn a_refusal_carries_its_pool_and_when_to_come_back() {
        let (status, headers, body) = parts(ApiError::refused(
            Refusal::Spent {
                retry_after_ms: 12_300,
            },
            "launcher",
        ))
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(headers[header::RETRY_AFTER], "13");
        assert_eq!(
            body,
            json!({
                "code": "BUDGET_SPENT",
                "message": "the gateway has asked CurseForge enough this minute",
                "pool": "launcher",
                "retryAfterMs": 12_300,
            })
        );
    }

    #[tokio::test]
    async fn a_request_too_large_for_any_minute_is_not_told_to_retry() {
        let (status, headers, body) = parts(ApiError::refused(
            Refusal::TooLarge {
                calls: 400,
                limit_per_minute: 300,
            },
            "jobs",
        ))
        .await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert!(headers.get(header::RETRY_AFTER).is_none());
        assert_eq!(body["code"], "REQUEST_TOO_LARGE");
        assert_eq!(body["pool"], "jobs");
        assert!(body.get("retryAfterMs").is_none());
    }

    #[tokio::test]
    async fn retry_after_is_at_least_one_second() {
        let (_, headers, body) = parts(ApiError::refused(
            Refusal::Hold { retry_after_ms: 40 },
            "jobs",
        ))
        .await;
        assert_eq!(headers[header::RETRY_AFTER], "1");
        assert_eq!(body["code"], "UPSTREAM_HOLD");
    }
}
