use axum::{Router, routing::get};

use crate::health;

pub fn router() -> Router {
    Router::new().route("/v1/healthz", get(health::healthz))
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

    async fn get(uri: &str) -> (StatusCode, Option<String>, Vec<u8>) {
        let response = router()
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
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
    async fn healthz_answers_ok_as_json() {
        let (status, content_type, body) = get("/v1/healthz").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type.as_deref(), Some("application/json"));

        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["status"], "ok");
        assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
        assert!(body.get("commit").is_some());
        assert!(body.get("builtAt").is_some());
    }

    #[tokio::test]
    async fn unknown_routes_answer_not_found() {
        let (status, _, _) = get("/v1/nothing-here").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
