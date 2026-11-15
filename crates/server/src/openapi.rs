use axum::Json;
use utoipa::{
    Modify, OpenApi,
    openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme},
};

use crate::{
    budget::{
        self, BudgetSnapshot, DayCounts, DayPoolSnapshot, DaySnapshot, PoolSnapshot, Source,
        UpstreamBudget,
    },
    curseforge,
    error::{ApiError, ErrorCode},
    health::{self, Health},
    modrinth::{self, VersionFilesRequest},
    resolve::{self, FileSource, ResolveRequest, Resolved, ResolvedFile},
    versions::{
        self, JavaRuntime, Latest, Loader, LoaderVersion, LoaderVersions, MinecraftVersion,
        MinecraftVersionDetails, MinecraftVersions, RuntimeManifest, VersionFilter, VersionType,
    },
};

/// The version of the API the document describes, the `1` of `/v1`.
pub const API_VERSION: &str = "1";

/// The OpenAPI document of the gateway's routes, served at `/v1/openapi.json` and checked in as `openapi.json`.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "content-gateway",
        version = API_VERSION,
        description = "A gateway in front of the CurseForge and Modrinth APIs that holds the CurseForge key, caches answers for every consumer, counts the calls made on the key and holds every consumer off after a 429. `/v1/curseforge/{path}` mirrors `api.curseforge.com/v1/{path}` and `/v1/modrinth/{path}` mirrors `api.modrinth.com/v2/{path}`, answering their JSON as it came; `/v1/resolve` finds a download address per CurseForge file; `/v1/minecraft/*` lists Minecraft and mod loader versions from Mojang, NeoForge, Forge and Fabric, and the Java runtimes Mojang ships; the other routes are the gateway's own. Every route but `/v1/healthz` and `/v1/openapi.json` needs a consumer's bearer token.",
        license(name = "MIT"),
    ),
    servers((url = "http://127.0.0.1:5005", description = "The default listen address")),
    tags(
        (name = "gateway", description = "The gateway's own routes"),
        (name = "curseforge", description = "The CurseForge passthrough"),
        (name = "modrinth", description = "The Modrinth passthrough"),
        (name = "minecraft", description = "Minecraft and mod loader version lists, and Mojang's Java runtimes"),
    ),
    paths(
        health::healthz,
        document,
        budget::readout,
        curseforge::passthrough_get,
        curseforge::passthrough_post,
        modrinth::passthrough_get,
        modrinth::passthrough_post,
        resolve::resolve,
        versions::minecraft_versions,
        versions::minecraft_version,
        versions::loader_versions,
        versions::java_runtime,
    ),
    components(schemas(
        Health,
        BudgetSnapshot,
        UpstreamBudget,
        PoolSnapshot,
        DaySnapshot,
        DayPoolSnapshot,
        DayCounts,
        Source,
        VersionFilesRequest,
        ResolveRequest,
        Resolved,
        ResolvedFile,
        FileSource,
        MinecraftVersions,
        Latest,
        MinecraftVersion,
        MinecraftVersionDetails,
        LoaderVersions,
        LoaderVersion,
        JavaRuntime,
        RuntimeManifest,
        Loader,
        VersionType,
        VersionFilter,
        ApiError,
        ErrorCode,
    )),
    modifiers(&Bearer)
)]
struct ApiDoc;

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

    use crate::upstream::Source;

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
    fn the_modrinth_path_descriptions_list_the_slug_characters_checked() {
        let document: Value = serde_json::from_str(CHECKED_IN).unwrap();
        let slug = format!("`{}`", Source::Modrinth.slug_characters());
        for method in ["get", "post"] {
            let parameters = document["paths"]["/v1/modrinth/{path}"][method]["parameters"]
                .as_array()
                .unwrap();
            let path = parameters
                .iter()
                .find(|parameter| parameter["name"] == "path")
                .unwrap();
            let description = path["description"].as_str().unwrap();
            assert!(description.contains(&slug), "{method}: {description}");
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
