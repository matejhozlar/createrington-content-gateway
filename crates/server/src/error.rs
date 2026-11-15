use axum::{
    Json,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Serialize;
use utoipa::ToSchema;

use crate::{
    budget::Refusal,
    upstream::{Source, UpstreamFailure},
};

/// What went wrong, for a consumer to branch on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    Unauthorized,
    PoolRequired,
    UnknownPool,
    InvalidPath,
    InvalidDays,
    InvalidHash,
    InvalidBody,
    InvalidQuery,
    UnknownVersion,
    UnknownLoader,
    UnknownRuntime,
    UpstreamTimeout,
    UpstreamUnreachable,
    UpstreamTooLarge,
    UpstreamUnreadable,
    BudgetSpent,
    UpstreamHold,
    RequestTooLarge,
}

/// The JSON error every route answers with: `{ code, message }`, plus `pool` and `retryAfterMs` on a refused call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiError {
    #[serde(skip)]
    status: StatusCode,
    code: ErrorCode,
    /// What happened, in one sentence for a log line.
    message: String,
    /// The pool the refused call would have drawn from, on a refused call and an unknown pool.
    #[serde(skip_serializing_if = "Option::is_none")]
    pool: Option<String>,
    /// When the next call can go out, on a refused call. `Retry-After` carries the same in whole seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    retry_after_ms: Option<u64>,
}

impl ApiError {
    pub fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            ErrorCode::Unauthorized,
            "send a consumer token as Authorization: Bearer <token>",
        )
    }

    pub fn pool_required(pools: &[impl AsRef<str>]) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            ErrorCode::PoolRequired,
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
                ErrorCode::UnknownPool,
                format!(
                    "this token cannot draw from pool {pool:?}, only from: {}",
                    join(pools)
                ),
            )
        }
    }

    pub fn invalid_path(source: Source) -> Self {
        let characters = match source.slug_characters() {
            "" => "- and _".to_owned(),
            slug => format!("-, _ and any of {slug}"),
        };
        Self::new(
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidPath,
            format!(
                "a {} path is segments of letters, digits, {characters}, separated by single slashes",
                source.name()
            ),
        )
    }

    pub fn invalid_days() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidDays,
            "days must be a whole number of UTC days from 1 up",
        )
    }

    pub fn invalid_hash() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidHash,
            "a SHA-1 is 40 hex digits",
        )
    }

    pub fn invalid_body(expected: &str) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidBody,
            format!("the body must be {expected}"),
        )
    }

    pub fn invalid_query(expected: &str) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidQuery,
            format!("the query must be {expected}"),
        )
    }

    pub fn unknown_version(id: &str) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            ErrorCode::UnknownVersion,
            format!("Mojang lists no Minecraft version {id:?}"),
        )
    }

    pub fn unknown_loader() -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            ErrorCode::UnknownLoader,
            "the loaders are neoforge, forge and fabric",
        )
    }

    pub fn unknown_runtime(what: &str, platform: &str) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            ErrorCode::UnknownRuntime,
            format!("Mojang ships no {what} for platform {platform:?}"),
        )
    }

    pub fn upstream(failure: UpstreamFailure, source: &str) -> Self {
        match failure {
            UpstreamFailure::Timeout => Self::new(
                StatusCode::GATEWAY_TIMEOUT,
                ErrorCode::UpstreamTimeout,
                format!("{source} did not answer in time"),
            ),
            UpstreamFailure::Unreachable => Self::new(
                StatusCode::BAD_GATEWAY,
                ErrorCode::UpstreamUnreachable,
                format!("{source} could not be reached"),
            ),
            UpstreamFailure::TooLarge => Self::new(
                StatusCode::BAD_GATEWAY,
                ErrorCode::UpstreamTooLarge,
                format!("{source} answered with more than the gateway keeps"),
            ),
            UpstreamFailure::Unreadable => Self::new(
                StatusCode::BAD_GATEWAY,
                ErrorCode::UpstreamUnreadable,
                format!("{source} answered with something the gateway cannot read"),
            ),
        }
    }

    pub fn refused(refusal: Refusal, pool: &str, source: &str) -> Self {
        let (error, retry_after_ms) = match refusal {
            Refusal::Spent { retry_after_ms } => (
                Self::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    ErrorCode::BudgetSpent,
                    format!("the gateway has asked {source} enough this minute"),
                ),
                Some(retry_after_ms),
            ),
            Refusal::Hold { retry_after_ms } => (
                Self::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    ErrorCode::UpstreamHold,
                    format!("{source} answered 429, every call waits until the hold ends"),
                ),
                Some(retry_after_ms),
            ),
            Refusal::TooLarge {
                calls,
                limit_per_minute,
            } => (
                Self::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    ErrorCode::RequestTooLarge,
                    format!(
                        "this request needs {calls} calls to {source}, more than the {limit_per_minute} a minute allows; split it"
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

    fn new(status: StatusCode, code: ErrorCode, message: impl Into<String>) -> Self {
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
    use axum::http::HeaderMap;
    use serde_json::{Value, json};

    use super::*;
    use crate::test_support::collect;

    async fn parts(error: ApiError) -> (StatusCode, HeaderMap, Value) {
        let got = collect(error.into_response()).await;
        let body = got.json();
        (got.status, got.headers, body)
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
            "CurseForge",
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
            "CurseForge",
        ))
        .await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert!(headers.get(header::RETRY_AFTER).is_none());
        assert_eq!(body["code"], "REQUEST_TOO_LARGE");
        assert_eq!(body["pool"], "jobs");
        assert!(body.get("retryAfterMs").is_none());
    }

    #[tokio::test]
    async fn an_upstream_failure_names_the_upstream_and_what_went_wrong() {
        let cases = [
            (
                UpstreamFailure::Timeout,
                StatusCode::GATEWAY_TIMEOUT,
                "UPSTREAM_TIMEOUT",
                "Modrinth did not answer in time",
            ),
            (
                UpstreamFailure::Unreachable,
                StatusCode::BAD_GATEWAY,
                "UPSTREAM_UNREACHABLE",
                "Modrinth could not be reached",
            ),
            (
                UpstreamFailure::TooLarge,
                StatusCode::BAD_GATEWAY,
                "UPSTREAM_TOO_LARGE",
                "Modrinth answered with more than the gateway keeps",
            ),
            (
                UpstreamFailure::Unreadable,
                StatusCode::BAD_GATEWAY,
                "UPSTREAM_UNREADABLE",
                "Modrinth answered with something the gateway cannot read",
            ),
        ];
        for (failure, status, code, message) in cases {
            let (got_status, headers, body) = parts(ApiError::upstream(failure, "Modrinth")).await;
            assert_eq!(got_status, status, "{failure:?}");
            assert!(headers.get(header::RETRY_AFTER).is_none(), "{failure:?}");
            assert_eq!(
                body,
                json!({ "code": code, "message": message }),
                "{failure:?}"
            );
        }
    }

    #[tokio::test]
    async fn an_unknown_pool_names_the_pool_asked_for_and_the_ones_allowed() {
        let (status, _, body) = parts(ApiError::unknown_pool("jobs", &["launcher", "app"])).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body,
            json!({
                "code": "UNKNOWN_POOL",
                "message": "this token cannot draw from pool \"jobs\", only from: launcher, app",
                "pool": "jobs",
            })
        );
    }

    #[tokio::test]
    async fn retry_after_is_at_least_one_second() {
        let (_, headers, body) = parts(ApiError::refused(
            Refusal::Hold { retry_after_ms: 40 },
            "jobs",
            "Modrinth",
        ))
        .await;
        assert_eq!(headers[header::RETRY_AFTER], "1");
        assert_eq!(body["code"], "UPSTREAM_HOLD");
    }
}
