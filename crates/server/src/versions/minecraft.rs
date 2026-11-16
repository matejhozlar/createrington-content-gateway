use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, Query, State, rejection::QueryRejection},
};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use super::{
    LIST_FRESH_MS, ListFailure, Origin, document,
    loaders::{Loader, supported},
};
use crate::{app::AppState, clock::DAY_MS, error::ApiError};

pub(super) const MANIFEST_PATH: &str = "/mc/game/version_manifest_v2.json";
const DETAILS_FRESH_MS: u64 = DAY_MS;

/// Mojang's kinds of versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum VersionType {
    Release,
    Snapshot,
    OldBeta,
    OldAlpha,
}

/// Which versions `GET /v1/minecraft/versions` lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum VersionFilter {
    #[default]
    Release,
    Snapshot,
    All,
}

/// The query of `GET /v1/minecraft/versions`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct VersionsQuery {
    /// `release` (the default), `snapshot` (snapshots only) or `all` (every kind, old betas and alphas included).
    #[serde(rename = "type")]
    pub kind: Option<VersionFilter>,
    /// Only the versions this loader has at least one build for.
    pub loader: Option<Loader>,
}

/// What `GET /v1/minecraft/versions` answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct MinecraftVersions {
    pub latest: Latest,
    /// Newest first, as Mojang lists them.
    pub versions: Vec<MinecraftVersion>,
}

/// The newest release and snapshot Mojang names, whatever the filter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct Latest {
    pub release: String,
    pub snapshot: String,
}

/// One Minecraft version as Mojang's manifest lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MinecraftVersion {
    /// Such as `1.21.1` or `24w33a`.
    pub id: String,
    #[serde(rename = "type")]
    pub kind: VersionType,
    /// ISO 8601.
    pub release_time: String,
}

/// What `GET /v1/minecraft/versions/{id}` answers: the version and the Java it needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MinecraftVersionDetails {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: VersionType,
    pub release_time: String,
    /// The major version of the Java the version runs on, such as 21, or null when Mojang does not say.
    pub java_major: Option<u32>,
    /// Mojang's runtime for it, such as `java-runtime-delta`, the `component` of `GET /v1/minecraft/java`; null when Mojang does not say.
    pub java_component: Option<String>,
}

#[derive(Deserialize)]
struct Manifest {
    latest: ManifestLatest,
    versions: Vec<ManifestVersion>,
}

#[derive(Deserialize)]
struct ManifestLatest {
    release: String,
    snapshot: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestVersion {
    id: String,
    #[serde(rename = "type")]
    kind: VersionType,
    url: String,
    release_time: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VersionDocument {
    java_version: Option<JavaVersion>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct JavaVersion {
    major_version: u32,
    #[serde(default)]
    component: Option<String>,
}

async fn manifest(state: &Arc<AppState>) -> Result<Manifest, ListFailure> {
    let url = format!("{}{MANIFEST_PATH}", state.meta.mojang);
    document(
        state,
        "minecraft:manifest",
        &url,
        Origin::Mojang,
        LIST_FRESH_MS,
    )
    .await
}

/// The Minecraft versions Mojang lists, releases unless asked otherwise, newest first.
///
/// `loader` keeps only the versions that loader has builds for, which is what a server or pack picker offers.
/// Mojang's manifest is kept for an hour and the old copy answers when Mojang fails.
#[utoipa::path(
    get,
    path = "/v1/minecraft/versions",
    tag = "minecraft",
    security(("bearer" = [])),
    params(VersionsQuery),
    responses(
        (status = 200, description = "The versions", body = MinecraftVersions),
        (status = 400, description = "`INVALID_QUERY`: an unknown `type` or `loader`", body = ApiError),
        (status = 401, description = "No valid consumer token", body = ApiError),
        (status = 500, description = "The gateway failed to finish the request: `INTERNAL`", body = ApiError),
        (status = 502, description = "Mojang or the loader's source could not be read: `UPSTREAM_UNREACHABLE`, `UPSTREAM_TOO_LARGE` or `UPSTREAM_UNREADABLE`", body = ApiError),
        (status = 504, description = "The source did not answer in time, `UPSTREAM_TIMEOUT`", body = ApiError),
    )
)]
pub async fn minecraft_versions(
    State(state): State<Arc<AppState>>,
    query: Result<Query<VersionsQuery>, QueryRejection>,
) -> Result<Json<MinecraftVersions>, ApiError> {
    let Ok(Query(query)) = query else {
        return Err(ApiError::invalid_query(
            "`type` is release, snapshot or all; `loader` is neoforge, forge or fabric",
        ));
    };
    let manifest = manifest(&state).await?;
    let with_builds = match query.loader {
        Some(loader) => Some(supported(&state, loader).await?),
        None => None,
    };
    let kind = query.kind.unwrap_or_default();
    let versions = manifest
        .versions
        .into_iter()
        .filter(|version| match kind {
            VersionFilter::Release => version.kind == VersionType::Release,
            VersionFilter::Snapshot => version.kind == VersionType::Snapshot,
            VersionFilter::All => true,
        })
        .filter(|version| {
            with_builds
                .as_ref()
                .is_none_or(|supported| supported.contains(&version.id))
        })
        .map(|version| MinecraftVersion {
            id: version.id,
            kind: version.kind,
            release_time: version.release_time,
        })
        .collect();
    Ok(Json(MinecraftVersions {
        latest: Latest {
            release: manifest.latest.release,
            snapshot: manifest.latest.snapshot,
        },
        versions,
    }))
}

/// One Minecraft version with the Java it needs, from Mojang's own document of the version, kept a day.
#[utoipa::path(
    get,
    path = "/v1/minecraft/versions/{id}",
    tag = "minecraft",
    security(("bearer" = [])),
    params(("id" = String, Path, description = "The version as Mojang names it", example = "1.21.1")),
    responses(
        (status = 200, description = "The version", body = MinecraftVersionDetails),
        (status = 401, description = "No valid consumer token", body = ApiError),
        (status = 404, description = "Mojang lists no such version, `UNKNOWN_VERSION`", body = ApiError),
        (status = 500, description = "The gateway failed to finish the request: `INTERNAL`", body = ApiError),
        (status = 502, description = "Mojang could not be read: `UPSTREAM_UNREACHABLE`, `UPSTREAM_TOO_LARGE` or `UPSTREAM_UNREADABLE`", body = ApiError),
        (status = 504, description = "Mojang did not answer in time, `UPSTREAM_TIMEOUT`", body = ApiError),
    )
)]
pub async fn minecraft_version(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<MinecraftVersionDetails>, ApiError> {
    let manifest = manifest(&state).await?;
    let Some(version) = manifest
        .versions
        .into_iter()
        .find(|version| version.id == id)
    else {
        return Err(ApiError::unknown_version(&id));
    };
    if !version.url.starts_with(&format!("{}/", state.meta.mojang)) {
        tracing::warn!(
            version = %id,
            url = %version.url,
            "Mojang's manifest points a version outside its own host"
        );
        return Err(ListFailure::unreadable(Origin::Mojang).into());
    }
    let key = format!("minecraft:version:{id}");
    let details: VersionDocument =
        document(&state, &key, &version.url, Origin::Mojang, DETAILS_FRESH_MS).await?;
    Ok(Json(MinecraftVersionDetails {
        id: version.id,
        kind: version.kind,
        release_time: version.release_time,
        java_major: details.java_version.as_ref().map(|java| java.major_version),
        java_component: details.java_version.and_then(|java| java.component),
    }))
}
