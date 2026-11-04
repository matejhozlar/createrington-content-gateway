use axum::Json;
use serde::Serialize;
use utoipa::ToSchema;

/// What `/v1/healthz` answers: the running gateway's version and build.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Health {
    /// Always `ok` when the gateway answers at all.
    #[schema(example = "ok")]
    status: &'static str,
    /// The gateway's version, as released.
    #[schema(example = "0.1.0")]
    version: &'static str,
    /// The commit the binary was built from, when the build said.
    commit: Option<&'static str>,
    /// When the binary was built, ISO 8601 in UTC, when the build said.
    built_at: Option<&'static str>,
}

/// Whether the gateway is up, and which build it is.
#[utoipa::path(
    get,
    path = "/v1/healthz",
    tag = "gateway",
    responses((status = 200, description = "The gateway is up", body = Health))
)]
pub async fn healthz() -> Json<Health> {
    Json(Health {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        commit: option_env!("CONTENT_GATEWAY_COMMIT"),
        built_at: option_env!("CONTENT_GATEWAY_BUILT_AT"),
    })
}
