use axum::Json;
use utoipa::{
    Modify, OpenApi,
    openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme},
};

use crate::{
    budget::{self, BudgetSnapshot, DayCounts, DayPoolSnapshot, DaySnapshot, PoolSnapshot, Source},
    curseforge,
    error::{ApiError, ErrorCode},
    health::{self, Health},
};

/// The version of the API the document describes, which is the `/v1` of every route. The gateway's own version is
/// what `/v1/healthz` reports.
pub const API_VERSION: &str = "1";

/// The OpenAPI document of the gateway's routes, served at `/v1/openapi.json` and checked in as `openapi.json`.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "content-gateway",
        version = API_VERSION,
        description = "A gateway in front of the CurseForge API that holds the API key, caches answers for every consumer, counts the calls made on the key and holds every consumer off after a 429. `/v1/curseforge/{path}` mirrors `api.curseforge.com/v1/{path}` and answers CurseForge's JSON as it came; the other routes are the gateway's own. Every route but `/v1/healthz` and `/v1/openapi.json` needs a consumer's bearer token.",
        license(name = "MIT"),
    ),
    servers((url = "http://127.0.0.1:5005", description = "The default listen address")),
    tags(
        (name = "gateway", description = "The gateway's own routes"),
        (name = "curseforge", description = "The CurseForge passthrough"),
    ),
    paths(
        health::healthz,
        document,
        budget::readout,
        curseforge::passthrough_get,
        curseforge::passthrough_post,
    ),
    components(schemas(
        Health,
        BudgetSnapshot,
        PoolSnapshot,
        DaySnapshot,
        DayPoolSnapshot,
        DayCounts,
        Source,
        ApiError,
        ErrorCode,
    )),
    modifiers(&Bearer)
)]
pub struct ApiDoc;

struct Bearer;

impl Modify for Bearer {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        if let Some(components) = openapi.components.as_mut() {
            components.add_security_scheme(
                "bearer",
                SecurityScheme::Http(
                    HttpBuilder::new()
                        .scheme(HttpAuthScheme::Bearer)
                        .description(Some("A consumer's token, from the gateway's configuration"))
                        .build(),
                ),
            );
        }
    }
}

/// This document.
#[utoipa::path(
    get,
    path = "/v1/openapi.json",
    tag = "gateway",
    responses((status = 200, description = "The OpenAPI document of every route", body = Value))
)]
pub async fn document() -> Json<utoipa::openapi::OpenApi> {
    Json(ApiDoc::openapi())
}

/// The document as `openapi.json` holds it: pretty-printed, keys sorted, one trailing newline.
pub fn pretty_json() -> Result<String, serde_json::Error> {
    let mut json = ApiDoc::openapi().to_pretty_json()?;
    json.push('\n');
    Ok(json)
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    const CHECKED_IN: &str = include_str!("../../../openapi.json");

    #[test]
    fn the_checked_in_document_is_current() {
        let generated = pretty_json().unwrap();
        assert!(
            generated == CHECKED_IN.replace("\r\n", "\n"),
            "openapi.json is stale, run: pnpm openapi"
        );
    }

    #[test]
    fn every_error_code_is_in_the_document() {
        let document: Value = serde_json::from_str(CHECKED_IN).unwrap();
        let codes = document["components"]["schemas"]["ErrorCode"]["enum"]
            .as_array()
            .unwrap();
        for code in [
            "UNAUTHORIZED",
            "POOL_REQUIRED",
            "UNKNOWN_POOL",
            "INVALID_PATH",
            "UPSTREAM_TIMEOUT",
            "UPSTREAM_UNREACHABLE",
            "UPSTREAM_TOO_LARGE",
            "UPSTREAM_UNREADABLE",
            "BUDGET_SPENT",
            "UPSTREAM_HOLD",
            "REQUEST_TOO_LARGE",
        ] {
            assert!(codes.contains(&Value::from(code)), "{code}");
        }
    }

    #[test]
    fn the_bearer_scheme_guards_every_route_but_health_and_the_document() {
        let document: Value = serde_json::from_str(CHECKED_IN).unwrap();
        assert_eq!(
            document["components"]["securitySchemes"]["bearer"]["scheme"],
            "bearer"
        );
        for (path, operations) in document["paths"].as_object().unwrap() {
            for (method, operation) in operations.as_object().unwrap() {
                let open = path == "/v1/healthz" || path == "/v1/openapi.json";
                assert_eq!(operation.get("security").is_none(), open, "{method} {path}");
            }
        }
    }
}
