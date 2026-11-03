use axum::Json;
use serde::Serialize;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Health {
    status: &'static str,
    version: &'static str,
    commit: Option<&'static str>,
    built_at: Option<&'static str>,
}

pub async fn healthz() -> Json<Health> {
    Json(Health {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        commit: option_env!("CONTENT_GATEWAY_COMMIT"),
        built_at: option_env!("CONTENT_GATEWAY_BUILT_AT"),
    })
}
