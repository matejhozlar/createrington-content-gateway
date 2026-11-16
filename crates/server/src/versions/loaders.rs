use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use axum::{
    Json,
    extract::{Path, Query, State, rejection::QueryRejection},
};
use serde::{Deserialize, Deserializer, Serialize};
use utoipa::{IntoParams, ToSchema};

use super::{LIST_FRESH_MS, ListFailure, Origin, document, version_order};
use crate::{app::AppState, error::ApiError};

pub(super) const NEOFORGE_PATH: &str = "/api/maven/versions/releases/net/neoforged/neoforge";
pub(super) const FORGE_PATH: &str = "/net/minecraftforge/forge/promotions_slim.json";
pub(super) const FABRIC_LOADER_PATH: &str = "/v2/versions/loader";
pub(super) const FABRIC_GAME_PATH: &str = "/v2/versions/game";
const FIRST_YEAR_VERSION: u32 = 26;

/// The mod loaders whose versions the gateway lists, named in any case in a path or a query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Loader {
    NeoForge,
    Forge,
    Fabric,
}

impl Loader {
    const NAMES: [&str; 3] = ["neoforge", "forge", "fabric"];

    fn of(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "neoforge" => Some(Self::NeoForge),
            "forge" => Some(Self::Forge),
            "fabric" => Some(Self::Fabric),
            _ => None,
        }
    }
}

impl<'de> Deserialize<'de> for Loader {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        Self::of(&name).ok_or_else(|| serde::de::Error::unknown_variant(&name, &Self::NAMES))
    }
}

/// The query of `GET /v1/minecraft/loaders/{loader}/versions`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct LoaderQuery {
    /// Only the builds for this Minecraft version, such as `1.21.1`.
    pub minecraft: Option<String>,
}

/// What `GET /v1/minecraft/loaders/{loader}/versions` answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct LoaderVersions {
    pub loader: Loader,
    /// The Minecraft version the list was narrowed to, or null for every build.
    pub minecraft: Option<String>,
    /// Newest first.
    pub versions: Vec<LoaderVersion>,
}

/// One build of a loader.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct LoaderVersion {
    /// Such as `21.1.57` (NeoForge), `52.0.1` (Forge) or `0.16.5` (Fabric).
    pub version: String,
    /// The Minecraft version the build is for; null for Fabric, whose loader works with every version it supports.
    pub minecraft: Option<String>,
    /// False for a beta or pre-release build.
    pub stable: bool,
    /// True for the build its maintainers recommend for the Minecraft version, where they say.
    pub recommended: bool,
}

#[derive(Deserialize)]
struct NeoForgeVersions {
    versions: Vec<String>,
}

#[derive(Deserialize)]
struct ForgePromotions {
    promos: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct FabricLoader {
    version: String,
    stable: bool,
}

#[derive(Deserialize)]
struct FabricGame {
    version: String,
}

async fn neoforge(state: &Arc<AppState>) -> Result<Vec<LoaderVersion>, ListFailure> {
    let url = format!("{}{NEOFORGE_PATH}", state.meta.neoforge);
    let listed: NeoForgeVersions = document(
        state,
        "loader:neoforge",
        &url,
        Origin::NeoForge,
        LIST_FRESH_MS,
    )
    .await?;
    Ok(listed
        .versions
        .into_iter()
        .rev()
        .map(|version| LoaderVersion {
            minecraft: neoforge_minecraft(&version),
            stable: !version.contains('-'),
            recommended: false,
            version,
        })
        .collect())
}

pub(super) fn neoforge_minecraft(version: &str) -> Option<String> {
    let mut parts = version.split(['.', '-']);
    let first: u32 = parts.next()?.parse().ok()?;
    let second: u32 = parts.next()?.parse().ok()?;
    if first < FIRST_YEAR_VERSION {
        return Some(if second == 0 {
            format!("1.{first}")
        } else {
            format!("1.{first}.{second}")
        });
    }
    let hotfix: u32 = parts.next()?.parse().ok()?;
    Some(if hotfix == 0 {
        format!("{first}.{second}")
    } else {
        format!("{first}.{second}.{hotfix}")
    })
}

async fn forge(state: &Arc<AppState>) -> Result<Vec<LoaderVersion>, ListFailure> {
    let url = format!("{}{FORGE_PATH}", state.meta.forge);
    let listed: ForgePromotions =
        document(state, "loader:forge", &url, Origin::Forge, LIST_FRESH_MS).await?;
    let mut versions: Vec<LoaderVersion> = Vec::new();
    for (key, version) in &listed.promos {
        let Some((minecraft, promotion)) = key.rsplit_once('-') else {
            continue;
        };
        let recommended = promotion == "recommended";
        if let Some(known) = versions.iter_mut().find(|known| {
            known.version == *version && known.minecraft.as_deref() == Some(minecraft)
        }) {
            known.recommended |= recommended;
            continue;
        }
        versions.push(LoaderVersion {
            version: version.clone(),
            minecraft: Some(minecraft.to_owned()),
            stable: true,
            recommended,
        });
    }
    versions.sort_by(|a, b| {
        version_order(b.minecraft.as_deref().unwrap_or_default())
            .cmp(&version_order(a.minecraft.as_deref().unwrap_or_default()))
            .then_with(|| version_order(&b.version).cmp(&version_order(&a.version)))
    });
    Ok(versions)
}

async fn fabric(state: &Arc<AppState>) -> Result<Vec<LoaderVersion>, ListFailure> {
    let url = format!("{}{FABRIC_LOADER_PATH}", state.meta.fabric);
    let listed: Vec<FabricLoader> =
        document(state, "loader:fabric", &url, Origin::Fabric, LIST_FRESH_MS).await?;
    Ok(listed
        .into_iter()
        .map(|loader| LoaderVersion {
            version: loader.version,
            minecraft: None,
            stable: loader.stable,
            recommended: false,
        })
        .collect())
}

async fn fabric_games(state: &Arc<AppState>) -> Result<BTreeSet<String>, ListFailure> {
    let url = format!("{}{FABRIC_GAME_PATH}", state.meta.fabric);
    let listed: Vec<FabricGame> = document(
        state,
        "loader:fabric:games",
        &url,
        Origin::Fabric,
        LIST_FRESH_MS,
    )
    .await?;
    Ok(listed.into_iter().map(|game| game.version).collect())
}

pub(super) async fn supported(
    state: &Arc<AppState>,
    loader: Loader,
) -> Result<BTreeSet<String>, ListFailure> {
    Ok(match loader {
        Loader::NeoForge => neoforge(state)
            .await?
            .into_iter()
            .filter_map(|version| version.minecraft)
            .collect(),
        Loader::Forge => forge(state)
            .await?
            .into_iter()
            .filter_map(|version| version.minecraft)
            .collect(),
        Loader::Fabric => fabric_games(state).await?,
    })
}

/// A loader's builds, newest first.
///
/// NeoForge's come from its Maven, Forge's are its promoted builds (the recommended and the latest per Minecraft
/// version, which is what pickers show), Fabric's are the loaders from its meta. `minecraft` keeps only the builds
/// for that version; for Fabric, whose loader is not tied to one, that means all of them when Fabric supports the
/// version and none otherwise. Each list is kept for an hour.
#[utoipa::path(
    get,
    path = "/v1/minecraft/loaders/{loader}/versions",
    tag = "minecraft",
    security(("bearer" = [])),
    params(
        ("loader" = String, Path, description = "`neoforge`, `forge` or `fabric`", example = "neoforge"),
        LoaderQuery,
    ),
    responses(
        (status = 200, description = "The builds", body = LoaderVersions),
        (status = 400, description = "`INVALID_QUERY`: `minecraft` given more than once", body = ApiError),
        (status = 401, description = "No valid consumer token", body = ApiError),
        (status = 404, description = "No such loader, `UNKNOWN_LOADER`", body = ApiError),
        (status = 500, description = "The gateway failed to finish the request: `INTERNAL`", body = ApiError),
        (status = 502, description = "The loader's source could not be read: `UPSTREAM_UNREACHABLE`, `UPSTREAM_TOO_LARGE` or `UPSTREAM_UNREADABLE`", body = ApiError),
        (status = 504, description = "The source did not answer in time, `UPSTREAM_TIMEOUT`", body = ApiError),
    )
)]
pub async fn loader_versions(
    State(state): State<Arc<AppState>>,
    Path(loader): Path<String>,
    query: Result<Query<LoaderQuery>, QueryRejection>,
) -> Result<Json<LoaderVersions>, ApiError> {
    let Some(loader) = Loader::of(&loader) else {
        return Err(ApiError::unknown_loader());
    };
    let Ok(Query(query)) = query else {
        return Err(ApiError::invalid_query(
            "at most one `minecraft`, such as 1.21.1",
        ));
    };
    let minecraft = query.minecraft.filter(|minecraft| !minecraft.is_empty());
    let versions = match loader {
        Loader::NeoForge => neoforge(&state).await?,
        Loader::Forge => forge(&state).await?,
        Loader::Fabric => match &minecraft {
            Some(minecraft) if !fabric_games(&state).await?.contains(minecraft) => Vec::new(),
            _ => fabric(&state).await?,
        },
    };
    let versions = versions
        .into_iter()
        .filter(|version| {
            version.minecraft.is_none()
                || minecraft
                    .as_ref()
                    .is_none_or(|wanted| version.minecraft.as_ref() == Some(wanted))
        })
        .collect();
    Ok(Json(LoaderVersions {
        loader,
        minecraft,
        versions,
    }))
}
