use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};

use axum::{
    Json,
    body::Bytes,
    extract::{Path, Query, State, rejection::QueryRejection},
    http::{Method, StatusCode},
};
use serde::{Deserialize, Deserializer, Serialize, de::DeserializeOwned};
use serde_json::value::RawValue;
use utoipa::{IntoParams, ToSchema};

use crate::{
    app::AppState,
    cache::{Answer, Entry, LoadFailure, Outcome},
    clock::{DAY_MS, HOUR_MS},
    error::ApiError,
    load,
    upstream::{self, UpstreamFailure},
};

pub const MOJANG_META: &str = "https://piston-meta.mojang.com";
pub const NEOFORGE_MAVEN: &str = "https://maven.neoforged.net";
pub const FORGE_FILES: &str = "https://files.minecraftforge.net";
pub const FABRIC_META: &str = "https://meta.fabricmc.net";
pub const TIMEOUT: Duration = Duration::from_secs(10);
const MANIFEST_PATH: &str = "/mc/game/version_manifest_v2.json";
const NEOFORGE_PATH: &str = "/api/maven/versions/releases/net/neoforged/neoforge";
const FORGE_PATH: &str = "/net/minecraftforge/forge/promotions_slim.json";
const FABRIC_LOADER_PATH: &str = "/v2/versions/loader";
const FABRIC_GAME_PATH: &str = "/v2/versions/game";
const JAVA_RUNTIMES_PATH: &str =
    "/v1/products/java-runtime/2ec0cc96c44e5a76b9c8b7c39df7210883d12871/all.json";
const SNAPSHOT_SUFFIX: &str = "-snapshot";
const LIST_FRESH_MS: u64 = HOUR_MS;
const FIRST_YEAR_VERSION: u32 = 26;
const DETAILS_FRESH_MS: u64 = DAY_MS;

/// The clients that read the public version lists, which need no key and no budget.
pub struct Meta {
    client: reqwest::Client,
    mojang: String,
    neoforge: String,
    forge: String,
    fabric: String,
}

impl Meta {
    pub fn new(
        mojang: impl Into<String>,
        neoforge: impl Into<String>,
        forge: impl Into<String>,
        fabric: impl Into<String>,
        user_agent: &str,
        timeout: Duration,
    ) -> reqwest::Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(timeout)
                .user_agent(user_agent)
                .build()?,
            mojang: mojang.into(),
            neoforge: neoforge.into(),
            forge: forge.into(),
            fabric: fabric.into(),
        })
    }

    async fn get(&self, url: &str, origin: Origin) -> Result<Bytes, UpstreamFailure> {
        let name = origin.name();
        let response = self
            .client
            .request(Method::GET, url)
            .send()
            .await
            .map_err(|error| upstream::failure(error, name))?;
        let sent = upstream::read(response, name).await?;
        if sent.answer.status != StatusCode::OK {
            tracing::warn!("{name} answered {} for {url}", sent.answer.status);
            return Err(UpstreamFailure::Unreadable);
        }
        Ok(sent.answer.body)
    }
}

/// Where a version list comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Mojang,
    NeoForge,
    Forge,
    Fabric,
}

impl Origin {
    pub fn name(self) -> &'static str {
        match self {
            Self::Mojang => "Mojang",
            Self::NeoForge => "NeoForge",
            Self::Forge => "Forge",
            Self::Fabric => "Fabric",
        }
    }
}

/// Why a version list could not be given: the source, and what went wrong with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{} {failure}", origin.name())]
pub struct ListFailure {
    pub origin: Origin,
    pub failure: UpstreamFailure,
}

impl ListFailure {
    fn unreadable(origin: Origin) -> Self {
        Self {
            origin,
            failure: UpstreamFailure::Unreadable,
        }
    }
}

impl From<ListFailure> for ApiError {
    fn from(failed: ListFailure) -> Self {
        ApiError::upstream(failed.failure, failed.origin.name())
    }
}

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

/// The query of `GET /v1/minecraft/loaders/{loader}/versions`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct LoaderQuery {
    /// Only the builds for this Minecraft version, such as `1.21.1`.
    pub minecraft: Option<String>,
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

/// The query of `GET /v1/minecraft/java`: a platform and either a Java major or a runtime component.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct JavaQuery {
    /// Mojang's name for the platform: `linux`, `linux-i386`, `mac-os`, `mac-os-arm64`, `windows-x64`, `windows-x86` or `windows-arm64`.
    pub platform: String,
    /// The newest runtime Mojang ships of this Java major, such as 21.
    pub major: Option<u32>,
    /// The runtime of this name, such as `java-runtime-delta`, as `javaComponent` of a version names it.
    pub component: Option<String>,
}

/// What `GET /v1/minecraft/java` answers: one Java runtime Mojang ships for a platform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct JavaRuntime {
    pub platform: String,
    /// Such as `java-runtime-delta`.
    pub component: String,
    /// Such as 21.
    pub major: u32,
    /// The Java version Mojang lists, such as `21.0.7` or `8u202`.
    pub version: String,
    /// When Mojang released this build of the runtime, ISO 8601, or null when Mojang does not say.
    pub released: Option<String>,
    pub manifest: RuntimeManifest,
}

/// Mojang's manifest of a runtime: every file to download, with its address, SHA-1 and size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct RuntimeManifest {
    pub url: String,
    /// The SHA-1 of the manifest itself.
    pub sha1: String,
    /// The size of the manifest in bytes.
    pub size: u64,
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

type RuntimeIndex = BTreeMap<String, Box<RawValue>>;

#[derive(Deserialize)]
struct RuntimeListing {
    manifest: ListedManifest,
    version: ListedRuntimeVersion,
}

#[derive(Deserialize)]
struct ListedManifest {
    url: String,
    sha1: String,
    size: u64,
}

#[derive(Deserialize)]
struct ListedRuntimeVersion {
    name: String,
    #[serde(default)]
    released: Option<String>,
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

fn parsed<T: DeserializeOwned>(body: &[u8], origin: Origin, url: &str) -> Result<T, ListFailure> {
    serde_json::from_slice(body).map_err(|error| {
        tracing::warn!(
            "{} answered {url} with something the gateway cannot read: {error}",
            origin.name()
        );
        ListFailure::unreadable(origin)
    })
}

fn read<T: DeserializeOwned>(entry: &Arc<Entry>) -> Option<T> {
    serde_json::from_slice(&entry.answer.body).ok()
}

/// The list at `url` read as `T`, from the cache while fresh. A body is kept only once it reads as `T`, and the old
/// copy answers while the source fails. A kept copy that does not read, such as one an older gateway kept in Redis,
/// counts as no copy. A fetched body is read twice, once to decide it is kept and once for each waiting request.
async fn document<T: DeserializeOwned + 'static>(
    state: &Arc<AppState>,
    key: &str,
    url: &str,
    origin: Origin,
    fresh_ms: u64,
) -> Result<T, ListFailure> {
    let fetch = {
        let (state, key, url) = (state.clone(), key.to_owned(), url.to_owned());
        async move {
            let body = match state.meta.get(&url, origin).await {
                Ok(body) => body,
                Err(failure) => return Outcome::called(Err(LoadFailure::Upstream(failure))),
            };
            if let Err(failed) = parsed::<T>(&body, origin, &url) {
                return Outcome::called(Err(LoadFailure::Upstream(failed.failure)));
            }
            let answer = Answer {
                status: StatusCode::OK,
                content_type: None,
                body,
            };
            let entry = Arc::new(Entry::new(answer, state.clock.now_ms(), fresh_ms));
            state.cache.insert(&key, entry.clone());
            Outcome::called(Ok(entry))
        }
    };
    match load::one(state, key.to_owned(), false, read::<T>, fetch).await {
        Ok(given) => Ok(given.value),
        Err(LoadFailure::Upstream(failure)) => Err(ListFailure { origin, failure }),
        Err(_) => Err(ListFailure {
            origin,
            failure: UpstreamFailure::Unreachable,
        }),
    }
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

fn neoforge_minecraft(version: &str) -> Option<String> {
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

fn version_order(version: &str) -> Vec<u64> {
    version
        .split(['.', '-'])
        .map(|part| part.parse().unwrap_or(0))
        .collect()
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

async fn supported(state: &Arc<AppState>, loader: Loader) -> Result<BTreeSet<String>, ListFailure> {
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
            "Mojang's manifest points version {id} outside its own host: {}",
            version.url
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

enum Wanted {
    Major(u32),
    Component(String),
}

fn runtime_major(name: &str) -> Option<u32> {
    let digits: String = name.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

fn first_listing(listings: &RawValue) -> Option<RuntimeListing> {
    let listings: Vec<&RawValue> = serde_json::from_str(listings.get()).ok()?;
    serde_json::from_str(listings.first()?.get()).ok()
}

fn pick(index: &RuntimeIndex, platform: &str, wanted: &Wanted) -> Option<JavaRuntime> {
    let components: BTreeMap<String, &RawValue> =
        serde_json::from_str(index.get(platform)?.get()).ok()?;
    components
        .into_iter()
        .filter_map(|(component, listings)| {
            let listing = first_listing(listings)?;
            let major = runtime_major(&listing.version.name)?;
            Some((component, major, listing))
        })
        .filter(|(component, major, _)| match wanted {
            Wanted::Major(wanted) => major == wanted && !component.ends_with(SNAPSHOT_SUFFIX),
            Wanted::Component(wanted) => component == wanted,
        })
        .max_by(|a, b| {
            version_order(&a.2.version.name)
                .cmp(&version_order(&b.2.version.name))
                .then_with(|| a.2.version.released.cmp(&b.2.version.released))
                .then_with(|| a.0.cmp(&b.0))
        })
        .map(|(component, major, listing)| JavaRuntime {
            platform: platform.to_owned(),
            component,
            major,
            version: listing.version.name,
            released: listing.version.released,
            manifest: RuntimeManifest {
                url: listing.manifest.url,
                sha1: listing.manifest.sha1,
                size: listing.manifest.size,
            },
        })
}

/// A Java runtime Mojang ships with its launcher, from Mojang's list of runtimes kept an hour.
///
/// `major` answers the newest runtime of that Java major (on equal versions the later released), `component` the runtime of that name, as `javaComponent`
/// of `GET /v1/minecraft/versions/{id}` names it. The answer points at Mojang's manifest of the runtime, which lists
/// every file with its address, SHA-1 and size; the files themselves never pass through the gateway.
#[utoipa::path(
    get,
    path = "/v1/minecraft/java",
    tag = "minecraft",
    security(("bearer" = [])),
    params(JavaQuery),
    responses(
        (status = 200, description = "The runtime", body = JavaRuntime),
        (status = 400, description = "`INVALID_QUERY`: no `platform`, or not exactly one of `major` and `component`", body = ApiError),
        (status = 401, description = "No valid consumer token", body = ApiError),
        (status = 404, description = "Mojang ships no such runtime for the platform, `UNKNOWN_RUNTIME`", body = ApiError),
        (status = 502, description = "Mojang could not be read: `UPSTREAM_UNREACHABLE`, `UPSTREAM_TOO_LARGE` or `UPSTREAM_UNREADABLE`", body = ApiError),
        (status = 504, description = "Mojang did not answer in time, `UPSTREAM_TIMEOUT`", body = ApiError),
    )
)]
pub async fn java_runtime(
    State(state): State<Arc<AppState>>,
    query: Result<Query<JavaQuery>, QueryRejection>,
) -> Result<Json<JavaRuntime>, ApiError> {
    let (platform, wanted) = match query {
        Ok(Query(JavaQuery {
            platform,
            major: Some(major),
            component: None,
        })) => (platform, Wanted::Major(major)),
        Ok(Query(JavaQuery {
            platform,
            major: None,
            component: Some(component),
        })) => (platform, Wanted::Component(component)),
        _ => {
            return Err(ApiError::invalid_query(
                "`platform` with either `major` or `component`",
            ));
        }
    };
    let url = format!("{}{JAVA_RUNTIMES_PATH}", state.meta.mojang);
    let index: RuntimeIndex = document(
        &state,
        "minecraft:java",
        &url,
        Origin::Mojang,
        LIST_FRESH_MS,
    )
    .await?;
    let Some(runtime) = pick(&index, &platform, &wanted) else {
        let what = match wanted {
            Wanted::Major(major) => format!("Java {major}"),
            Wanted::Component(component) => format!("runtime {component:?}"),
        };
        return Err(ApiError::unknown_runtime(&what, &platform));
    };
    if !runtime
        .manifest
        .url
        .starts_with(&format!("{}/", state.meta.mojang))
    {
        tracing::warn!(
            "Mojang's list of runtimes points {} outside its own host: {}",
            runtime.component,
            runtime.manifest.url
        );
        return Err(ListFailure::unreadable(Origin::Mojang).into());
    }
    Ok(Json(runtime))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::{app::Timeouts, test_support::*};

    fn manifest_body(base: &str) -> String {
        json!({
            "latest": { "release": "1.21.1", "snapshot": "24w33a" },
            "versions": [
                { "id": "24w33a", "type": "snapshot", "url": format!("{base}/v1/packages/a/24w33a.json"), "time": "x", "releaseTime": "2024-08-15T00:00:00+00:00", "sha1": "a", "complianceLevel": 1 },
                { "id": "1.21.1", "type": "release", "url": format!("{base}/v1/packages/b/1.21.1.json"), "time": "x", "releaseTime": "2024-08-08T00:00:00+00:00", "sha1": "b", "complianceLevel": 1 },
                { "id": "1.21", "type": "release", "url": format!("{base}/v1/packages/c/1.21.json"), "time": "x", "releaseTime": "2024-06-13T00:00:00+00:00", "sha1": "c", "complianceLevel": 1 },
                { "id": "1.20.1", "type": "release", "url": format!("{base}/v1/packages/d/1.20.1.json"), "time": "x", "releaseTime": "2023-06-12T00:00:00+00:00", "sha1": "d", "complianceLevel": 1 },
                { "id": "b1.7.3", "type": "old_beta", "url": format!("{base}/v1/packages/e/b1.7.3.json"), "time": "x", "releaseTime": "2011-07-11T00:00:00+00:00", "sha1": "e", "complianceLevel": 0 }
            ]
        })
        .to_string()
    }

    fn sources(call: usize, seen: &Seen) -> Reply {
        let base = seen.base.clone();
        match seen.uri.as_str() {
            MANIFEST_PATH => reply(200, manifest_body(&base)),
            "/v1/packages/b/1.21.1.json" => reply(
                200,
                r#"{"id":"1.21.1","javaVersion":{"component":"java-runtime-delta","majorVersion":21}}"#,
            ),
            "/v1/packages/e/b1.7.3.json" => reply(200, r#"{"id":"b1.7.3"}"#),
            NEOFORGE_PATH => reply(
                200,
                r#"{"isSnapshot":false,"versions":["20.4.80","21.0.0-beta","21.0.167","21.1.57","21.1.72"]}"#,
            ),
            FORGE_PATH => reply(
                200,
                r#"{"homepage":"x","promos":{"1.20.1-latest":"47.3.0","1.20.1-recommended":"47.2.0","1.21.1-latest":"52.0.1","1.21.1-recommended":"52.0.1"}}"#,
            ),
            FABRIC_LOADER_PATH => reply(
                200,
                r#"[{"version":"0.16.5","stable":true},{"version":"0.16.4","stable":true},{"version":"0.16.0-rc.1","stable":false}]"#,
            ),
            FABRIC_GAME_PATH => reply(
                200,
                r#"[{"version":"24w33a","stable":false},{"version":"1.21.1","stable":true},{"version":"1.21","stable":true}]"#,
            ),
            uri => {
                let _ = call;
                panic!("unexpected {uri}")
            }
        }
    }

    fn ids(body: &Value) -> Vec<&str> {
        body["versions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|version| {
                version["id"]
                    .as_str()
                    .or_else(|| version["version"].as_str())
                    .unwrap()
            })
            .collect()
    }

    #[tokio::test]
    async fn lists_releases_by_default_snapshots_or_everything_on_request_and_keeps_the_manifest() {
        let mock = Mock::start(Duration::ZERO, sources).await;
        let gateway = gateway(&mock);

        let got = get(&gateway.router, "/v1/minecraft/versions").await;
        assert_eq!(got.status, StatusCode::OK);
        let body = got.json();
        assert_eq!(body["latest"]["release"], "1.21.1");
        assert_eq!(body["latest"]["snapshot"], "24w33a");
        assert_eq!(ids(&body), ["1.21.1", "1.21", "1.20.1"]);
        assert_eq!(body["versions"][0]["type"], "release");
        assert_eq!(
            body["versions"][0]["releaseTime"],
            "2024-08-08T00:00:00+00:00"
        );

        let snapshots = get(&gateway.router, "/v1/minecraft/versions?type=snapshot").await;
        assert_eq!(ids(&snapshots.json()), ["24w33a"]);
        let all = get(&gateway.router, "/v1/minecraft/versions?type=all").await;
        assert_eq!(ids(&all.json()).len(), 5);
        assert_eq!(mock.calls(), 1);

        gateway.clock.advance(LIST_FRESH_MS + 1);
        get(&gateway.router, "/v1/minecraft/versions").await;
        assert_eq!(mock.calls(), 2);

        let bad = get(&gateway.router, "/v1/minecraft/versions?type=beta").await;
        assert_eq!(bad.status, StatusCode::BAD_REQUEST);
        assert_eq!(bad.json()["code"], "INVALID_QUERY");
        assert_eq!(gateway.state.curseforge_budget.snapshot().await.days, []);
    }

    #[tokio::test]
    async fn narrows_the_versions_to_the_ones_a_loader_builds_for() {
        let mock = Mock::start(Duration::ZERO, sources).await;
        let gateway = gateway(&mock);

        let neoforge = get(&gateway.router, "/v1/minecraft/versions?loader=neoforge").await;
        assert_eq!(ids(&neoforge.json()), ["1.21.1", "1.21"]);
        let forge = get(&gateway.router, "/v1/minecraft/versions?loader=forge").await;
        assert_eq!(ids(&forge.json()), ["1.21.1", "1.20.1"]);
        let fabric = get(
            &gateway.router,
            "/v1/minecraft/versions?loader=fabric&type=all",
        )
        .await;
        assert_eq!(ids(&fabric.json()), ["24w33a", "1.21.1", "1.21"]);
        assert_eq!(mock.calls(), 4);

        let any_case = get(&gateway.router, "/v1/minecraft/versions?loader=NeoForge").await;
        assert_eq!(ids(&any_case.json()), ["1.21.1", "1.21"]);
        let unknown = get(&gateway.router, "/v1/minecraft/versions?loader=quilt").await;
        assert_eq!(unknown.json()["code"], "INVALID_QUERY");
    }

    #[tokio::test]
    async fn a_version_tells_the_java_it_needs_and_is_kept_a_day() {
        let mock = Mock::start(Duration::ZERO, sources).await;
        let gateway = gateway(&mock);

        let got = get(&gateway.router, "/v1/minecraft/versions/1.21.1").await;
        assert_eq!(got.status, StatusCode::OK);
        assert_eq!(
            got.json(),
            json!({ "id": "1.21.1", "type": "release", "releaseTime": "2024-08-08T00:00:00+00:00", "javaMajor": 21, "javaComponent": "java-runtime-delta" })
        );
        get(&gateway.router, "/v1/minecraft/versions/1.21.1").await;
        assert_eq!(mock.calls(), 2);

        let old = get(&gateway.router, "/v1/minecraft/versions/b1.7.3").await;
        assert_eq!(old.json()["javaMajor"], Value::Null);
        assert_eq!(old.json()["javaComponent"], Value::Null);

        let unknown = get(&gateway.router, "/v1/minecraft/versions/1.99").await;
        assert_eq!(unknown.status, StatusCode::NOT_FOUND);
        assert_eq!(unknown.json()["code"], "UNKNOWN_VERSION");
    }

    #[tokio::test]
    async fn lists_a_loader_s_builds_newest_first_and_narrows_them_to_a_minecraft_version() {
        let mock = Mock::start(Duration::ZERO, sources).await;
        let gateway = gateway(&mock);

        let neoforge = get(&gateway.router, "/v1/minecraft/loaders/neoforge/versions").await;
        assert_eq!(neoforge.status, StatusCode::OK);
        let body = neoforge.json();
        assert_eq!(body["loader"], "neoforge");
        assert_eq!(body["minecraft"], Value::Null);
        assert_eq!(
            ids(&body),
            ["21.1.72", "21.1.57", "21.0.167", "21.0.0-beta", "20.4.80"]
        );
        assert_eq!(body["versions"][0]["minecraft"], "1.21.1");
        assert_eq!(body["versions"][2]["minecraft"], "1.21");
        assert_eq!(body["versions"][3]["stable"], false);

        let narrowed = get(
            &gateway.router,
            "/v1/minecraft/loaders/NeoForge/versions?minecraft=1.21.1",
        )
        .await;
        assert_eq!(ids(&narrowed.json()), ["21.1.72", "21.1.57"]);
        assert_eq!(narrowed.json()["minecraft"], "1.21.1");

        let forge = get(
            &gateway.router,
            "/v1/minecraft/loaders/forge/versions?minecraft=1.20.1",
        )
        .await;
        let body = forge.json();
        assert_eq!(ids(&body), ["47.3.0", "47.2.0"]);
        assert_eq!(body["versions"][0]["recommended"], false);
        assert_eq!(body["versions"][1]["recommended"], true);
        let forge_all = get(&gateway.router, "/v1/minecraft/loaders/forge/versions").await;
        let body = forge_all.json();
        assert_eq!(ids(&body), ["52.0.1", "47.3.0", "47.2.0"]);
        assert_eq!(body["versions"][0]["recommended"], true);

        let fabric = get(&gateway.router, "/v1/minecraft/loaders/fabric/versions").await;
        assert_eq!(ids(&fabric.json()), ["0.16.5", "0.16.4", "0.16.0-rc.1"]);
        assert_eq!(fabric.json()["versions"][2]["stable"], false);
        let fabric_for = get(
            &gateway.router,
            "/v1/minecraft/loaders/fabric/versions?minecraft=1.21.1",
        )
        .await;
        assert_eq!(ids(&fabric_for.json()).len(), 3);
        let fabric_none = get(
            &gateway.router,
            "/v1/minecraft/loaders/fabric/versions?minecraft=1.7.10",
        )
        .await;
        assert_eq!(ids(&fabric_none.json()), Vec::<&str>::new());

        let unknown = get(&gateway.router, "/v1/minecraft/loaders/quilt/versions").await;
        assert_eq!(unknown.status, StatusCode::NOT_FOUND);
        assert_eq!(unknown.json()["code"], "UNKNOWN_LOADER");
    }

    #[tokio::test]
    async fn a_source_that_fails_answers_the_old_list_or_a_502() {
        let mock = Mock::start(Duration::ZERO, |call, seen| {
            if call >= 2 {
                reply(503, "")
            } else {
                sources(call, seen)
            }
        })
        .await;
        let gateway = gateway(&mock);
        get(&gateway.router, "/v1/minecraft/versions").await;
        get(&gateway.router, "/v1/minecraft/loaders/fabric/versions").await;

        gateway.clock.advance(LIST_FRESH_MS + 1);
        let stale = get(&gateway.router, "/v1/minecraft/versions").await;
        assert_eq!(stale.status, StatusCode::OK);
        assert_eq!(ids(&stale.json()), ["1.21.1", "1.21", "1.20.1"]);

        let fresh = get(&gateway.router, "/v1/minecraft/loaders/forge/versions").await;
        assert_eq!(fresh.status, StatusCode::BAD_GATEWAY);
        assert_eq!(fresh.json()["code"], "UPSTREAM_UNREADABLE");
        assert!(fresh.json()["message"].as_str().unwrap().contains("Forge"));
    }

    #[test]
    fn maps_neoforge_builds_to_their_minecraft_version() {
        assert_eq!(neoforge_minecraft("21.1.57").as_deref(), Some("1.21.1"));
        assert_eq!(neoforge_minecraft("21.0.0-beta").as_deref(), Some("1.21"));
        assert_eq!(neoforge_minecraft("20.4.80").as_deref(), Some("1.20.4"));
        assert_eq!(neoforge_minecraft("26.1.0.12").as_deref(), Some("26.1"));
        assert_eq!(
            neoforge_minecraft("26.1.1.3-alpha").as_deref(),
            Some("26.1.1")
        );
        assert_eq!(neoforge_minecraft("26.1"), None);
        assert_eq!(neoforge_minecraft("nope"), None);
    }

    #[tokio::test]
    async fn year_based_builds_and_alpha_builds_are_listed_right() {
        let mock = Mock::start(Duration::ZERO, |_, seen| match seen.uri.as_str() {
            NEOFORGE_PATH => reply(
                200,
                r#"{"isSnapshot":false,"versions":["21.1.72","26.1.0.1-alpha","26.1.0.12","26.1.1.3"]}"#,
            ),
            MANIFEST_PATH => reply(
                200,
                json!({
                    "latest": { "release": "26.1.1", "snapshot": "26.1.1" },
                    "versions": [
                        { "id": "26.1.1", "type": "release", "url": "x", "time": "x", "releaseTime": "2026-05-01T00:00:00+00:00", "sha1": "a", "complianceLevel": 1 },
                        { "id": "26.1", "type": "release", "url": "x", "time": "x", "releaseTime": "2026-03-01T00:00:00+00:00", "sha1": "b", "complianceLevel": 1 },
                        { "id": "1.21.1", "type": "release", "url": "x", "time": "x", "releaseTime": "2024-08-08T00:00:00+00:00", "sha1": "c", "complianceLevel": 1 }
                    ]
                })
                .to_string(),
            ),
            uri => panic!("unexpected {uri}"),
        })
        .await;
        let gateway = gateway(&mock);

        let builds = get(
            &gateway.router,
            "/v1/minecraft/loaders/neoforge/versions?minecraft=26.1",
        )
        .await;
        let body = builds.json();
        assert_eq!(ids(&body), ["26.1.0.12", "26.1.0.1-alpha"]);
        assert_eq!(body["versions"][0]["stable"], true);
        assert_eq!(body["versions"][1]["stable"], false);

        let versions = get(&gateway.router, "/v1/minecraft/versions?loader=neoforge").await;
        assert_eq!(ids(&versions.json()), ["26.1.1", "26.1", "1.21.1"]);
    }

    fn runtime(base: &str, name: &str, sha1: &str) -> Value {
        json!([{
            "availability": { "group": 1, "progress": 100 },
            "manifest": { "sha1": sha1, "size": 82843, "url": format!("{base}/v1/packages/{sha1}/manifest.json") },
            "version": { "name": name, "released": "2025-05-19T08:30:12+00:00" }
        }])
    }

    fn runtimes(_: usize, seen: &Seen) -> Reply {
        let base = seen.base.as_str();
        assert_eq!(seen.uri, JAVA_RUNTIMES_PATH);
        reply(
            200,
            json!({
                "gamecore": { "java-runtime-delta": [], "jre-legacy": [] },
                "linux": {
                    "java-runtime-alpha": runtime(base, "16.0.1.9.1", "a"),
                    "java-runtime-beta": runtime(base, "17.0.15", "b"),
                    "java-runtime-delta": runtime(base, "21.0.7", "d"),
                    "java-runtime-gamma": runtime(base, "17.0.15", "g"),
                    "java-runtime-gamma-snapshot": runtime(base, "17.0.16", "s"),
                    "jre-legacy": runtime(base, "8u202", "l"),
                    "java-runtime-zeta": [{ "manifest": { "size": "large" }, "version": { "name": "21.0.9" } }],
                    "minecraft-java-exe": []
                },
                "windows-x64": { "java-runtime-delta": [{ "version": { "name": "21.0.7" } }] },
                "broken": 5,
                "windows-x86": {
                    "java-runtime-beta": runtime(base, "17.0.1.12.1", "wb"),
                    "java-runtime-gamma": runtime(base, "17.0.8.1", "wg")
                }
            })
            .to_string(),
        )
    }

    #[tokio::test]
    async fn a_java_runtime_is_found_by_major_or_component_from_one_kept_list() {
        let mock = Mock::start(Duration::ZERO, runtimes).await;
        let gateway = gateway(&mock);

        let got = get(
            &gateway.router,
            "/v1/minecraft/java?platform=linux&major=21",
        )
        .await;
        assert_eq!(got.status, StatusCode::OK);
        assert_eq!(
            got.json(),
            json!({
                "platform": "linux",
                "component": "java-runtime-delta",
                "major": 21,
                "version": "21.0.7",
                "released": "2025-05-19T08:30:12+00:00",
                "manifest": { "url": format!("{}/v1/packages/d/manifest.json", mock.base), "sha1": "d", "size": 82843 }
            })
        );

        let legacy = get(&gateway.router, "/v1/minecraft/java?platform=linux&major=8").await;
        assert_eq!(legacy.json()["component"], "jre-legacy");
        assert_eq!(legacy.json()["version"], "8u202");
        let seventeen = get(
            &gateway.router,
            "/v1/minecraft/java?platform=linux&major=17",
        )
        .await;
        assert_eq!(seventeen.json()["component"], "java-runtime-gamma");
        let older = get(
            &gateway.router,
            "/v1/minecraft/java?platform=windows-x86&major=17",
        )
        .await;
        assert_eq!(older.json()["component"], "java-runtime-gamma");
        assert_eq!(older.json()["version"], "17.0.8.1");

        let named = get(
            &gateway.router,
            "/v1/minecraft/java?platform=linux&component=java-runtime-gamma-snapshot",
        )
        .await;
        assert_eq!(named.json()["version"], "17.0.16");
        assert_eq!(named.json()["major"], 17);
        assert_eq!(mock.calls(), 1);

        gateway.clock.advance(LIST_FRESH_MS + 1);
        get(
            &gateway.router,
            "/v1/minecraft/java?platform=linux&major=21",
        )
        .await;
        assert_eq!(mock.calls(), 2);
        assert_eq!(gateway.state.curseforge_budget.snapshot().await.days, []);
    }

    #[tokio::test]
    async fn a_java_runtime_mojang_does_not_ship_or_a_bad_query_is_refused() {
        let mock = Mock::start(Duration::ZERO, runtimes).await;
        let gateway = gateway(&mock);

        for missing in [
            "/v1/minecraft/java?platform=linux&major=25",
            "/v1/minecraft/java?platform=gamecore&major=21",
            "/v1/minecraft/java?platform=solaris&major=21",
            "/v1/minecraft/java?platform=linux&component=minecraft-java-exe",
            "/v1/minecraft/java?platform=linux&component=java-runtime-zeta",
            "/v1/minecraft/java?platform=windows-x64&major=21",
            "/v1/minecraft/java?platform=broken&major=21",
        ] {
            let got = get(&gateway.router, missing).await;
            assert_eq!(got.status, StatusCode::NOT_FOUND, "{missing}");
            assert_eq!(got.json()["code"], "UNKNOWN_RUNTIME");
        }

        for bad in [
            "/v1/minecraft/java?major=21",
            "/v1/minecraft/java?platform=linux",
            "/v1/minecraft/java?platform=linux&major=21&component=jre-legacy",
            "/v1/minecraft/java?platform=linux&major=new",
        ] {
            let got = get(&gateway.router, bad).await;
            assert_eq!(got.status, StatusCode::BAD_REQUEST, "{bad}");
            assert_eq!(got.json()["code"], "INVALID_QUERY");
        }
    }

    #[tokio::test]
    async fn a_java_runtime_manifest_is_taken_from_mojang_s_host_only() {
        let mock = Mock::start(Duration::ZERO, |_, seen| {
            let elsewhere = format!("{}.example.net", seen.base);
            reply(
                200,
                json!({ "linux": { "java-runtime-delta": runtime(&elsewhere, "21.0.7", "d") } })
                    .to_string(),
            )
        })
        .await;
        let gateway = gateway(&mock);
        let got = get(
            &gateway.router,
            "/v1/minecraft/java?platform=linux&major=21",
        )
        .await;
        assert_eq!(got.status, StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn equal_runtime_versions_go_to_the_later_release_before_the_name() {
        let listing = |sha1: &str, released: &str| {
            json!([{
                "manifest": { "sha1": sha1, "size": 1, "url": "x" },
                "version": { "name": "21.0.7", "released": released }
            }])
        };
        let index: RuntimeIndex = serde_json::from_value(json!({
            "linux": {
                "java-runtime-delta": listing("d", "2025-06-01T00:00:00+00:00"),
                "java-runtime-gamma": listing("g", "2025-05-01T00:00:00+00:00")
            }
        }))
        .unwrap();
        let picked = pick(&index, "linux", &Wanted::Major(21)).unwrap();
        assert_eq!(picked.component, "java-runtime-delta");
    }

    #[test]
    fn reads_the_java_major_of_mojang_s_runtime_names() {
        assert_eq!(runtime_major("21.0.7"), Some(21));
        assert_eq!(runtime_major("8u202"), Some(8));
        assert_eq!(runtime_major("16.0.1.9.1_3"), Some(16));
        assert_eq!(runtime_major("jre"), None);
    }

    #[tokio::test]
    async fn a_version_document_is_read_from_mojang_s_host_only() {
        let mock = Mock::start(Duration::ZERO, |_, seen| match seen.uri.as_str() {
            MANIFEST_PATH => {
                let elsewhere = format!("{}.example.net/v1/packages/x.json", seen.base);
                reply(
                    200,
                    json!({
                        "latest": { "release": "1.21.1", "snapshot": "1.21.1" },
                        "versions": [{ "id": "1.21.1", "type": "release", "url": elsewhere, "time": "x", "releaseTime": "2024-08-08T00:00:00+00:00", "sha1": "a", "complianceLevel": 1 }]
                    })
                    .to_string(),
                )
            }
            uri => panic!("unexpected {uri}"),
        })
        .await;
        let gateway = gateway(&mock);
        let got = get(&gateway.router, "/v1/minecraft/versions/1.21.1").await;
        assert_eq!(got.status, StatusCode::BAD_GATEWAY);
        assert_eq!(mock.calls(), 1);
    }

    #[tokio::test]
    async fn a_200_the_gateway_cannot_read_is_a_502_and_not_kept() {
        let mock = Mock::start(Duration::ZERO, |_, _| reply(200, "not json")).await;
        let gateway = gateway(&mock);
        for _ in 0..2 {
            let got = get(&gateway.router, "/v1/minecraft/versions").await;
            assert_eq!(got.status, StatusCode::BAD_GATEWAY);
            assert_eq!(got.json()["code"], "UPSTREAM_UNREADABLE");
            assert!(got.json()["message"].as_str().unwrap().contains("Mojang"));
        }
        assert_eq!(mock.calls(), 2);
    }

    #[tokio::test]
    async fn an_unreadable_200_after_a_good_list_answers_the_old_list() {
        let mock = Mock::start(Duration::ZERO, |call, seen| {
            if call == 0 {
                sources(call, seen)
            } else {
                reply(200, r#"{"versions":"not a list"}"#)
            }
        })
        .await;
        let gateway = gateway(&mock);
        let fresh = get(&gateway.router, "/v1/minecraft/loaders/neoforge/versions").await;

        gateway.clock.advance(LIST_FRESH_MS + 1);
        for _ in 0..2 {
            let stale = get(&gateway.router, "/v1/minecraft/loaders/neoforge/versions").await;
            assert_eq!(stale.status, StatusCode::OK);
            assert_eq!(stale.json(), fresh.json());
        }
        assert_eq!(mock.calls(), 3);
    }

    #[tokio::test]
    async fn a_kept_copy_that_does_not_read_counts_as_no_copy() {
        let (mock, _never) = Mock::held_when(|seen| seen.uri == NEOFORGE_PATH, sources).await;
        let gateway = gateway_with(
            &mock,
            GatewayWith {
                timeouts: Timeouts {
                    meta: Duration::from_millis(50),
                    ..Timeouts::default()
                },
                ..GatewayWith::default()
            },
        );
        let unreadable = |fresh_ms| {
            Arc::new(Entry::new(
                Answer {
                    status: StatusCode::OK,
                    content_type: None,
                    body: Bytes::from_static(b"not json"),
                },
                NOV_3_2026_20H,
                fresh_ms,
            ))
        };
        gateway
            .state
            .cache
            .insert("loader:forge", unreadable(LIST_FRESH_MS));
        let fresh = get(&gateway.router, "/v1/minecraft/loaders/forge/versions").await;
        assert_eq!(fresh.status, StatusCode::OK);
        assert_eq!(mock.calls(), 1);

        gateway.state.cache.insert("loader:neoforge", unreadable(0));
        let failed = get(&gateway.router, "/v1/minecraft/loaders/neoforge/versions").await;
        assert_eq!(failed.status, StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(failed.json()["code"], "UPSTREAM_TIMEOUT");
        assert_eq!(mock.calls(), 2);
    }

    #[tokio::test]
    async fn a_loader_query_that_does_not_read_is_refused() {
        let mock = Mock::start(Duration::ZERO, sources).await;
        let gateway = gateway(&mock);
        let got = get(
            &gateway.router,
            "/v1/minecraft/loaders/forge/versions?minecraft=1.20.1&minecraft=1.21.1",
        )
        .await;
        assert_eq!(got.status, StatusCode::BAD_REQUEST);
        assert_eq!(got.json()["code"], "INVALID_QUERY");
        assert_eq!(mock.calls(), 0);
    }

    #[tokio::test]
    async fn a_source_that_does_not_answer_in_time_is_a_504() {
        let (mock, _never) = Mock::held(sources).await;
        let gateway = gateway_with(
            &mock,
            GatewayWith {
                timeouts: Timeouts {
                    meta: Duration::from_millis(50),
                    ..Timeouts::default()
                },
                ..GatewayWith::default()
            },
        );
        let got = get(&gateway.router, "/v1/minecraft/loaders/neoforge/versions").await;
        assert_eq!(got.status, StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(got.json()["code"], "UPSTREAM_TIMEOUT");
    }

    #[tokio::test]
    async fn requests_for_a_list_already_being_fetched_wait_for_it() {
        let (mock, gate) = Mock::held(sources).await;
        let gateway = gateway(&mock);
        let first = {
            let router = gateway.router.clone();
            tokio::spawn(async move { get(&router, "/v1/minecraft/versions").await })
        };
        until("the manifest call to reach Mojang", || mock.calls() == 1).await;
        let second = {
            let router = gateway.router.clone();
            tokio::spawn(async move { get(&router, "/v1/minecraft/versions?type=all").await })
        };
        until("the second request joining it", || {
            gateway.state.cache.joined() == 1
        })
        .await;
        gate.open();

        assert_eq!(first.await.unwrap().status, StatusCode::OK);
        let second = second.await.unwrap();
        assert_eq!(second.status, StatusCode::OK);
        assert_eq!(ids(&second.json()).len(), 5);
        assert_eq!(mock.calls(), 1);
    }

    #[tokio::test]
    async fn a_loader_list_is_answered_old_when_its_source_fails() {
        let mock = Mock::start(Duration::ZERO, |call, seen| {
            if call >= 1 {
                reply(503, "")
            } else {
                sources(call, seen)
            }
        })
        .await;
        let gateway = gateway(&mock);
        let fresh = get(&gateway.router, "/v1/minecraft/loaders/neoforge/versions").await;

        gateway.clock.advance(LIST_FRESH_MS + 1);
        let stale = get(&gateway.router, "/v1/minecraft/loaders/neoforge/versions").await;
        assert_eq!(stale.status, StatusCode::OK);
        assert_eq!(stale.json(), fresh.json());
        assert_eq!(mock.calls(), 2);
    }

    #[tokio::test]
    async fn an_empty_minecraft_version_lists_every_build() {
        let mock = Mock::start(Duration::ZERO, sources).await;
        let gateway = gateway(&mock);
        let every = get(&gateway.router, "/v1/minecraft/loaders/forge/versions").await;
        let empty = get(
            &gateway.router,
            "/v1/minecraft/loaders/forge/versions?minecraft=",
        )
        .await;
        assert_eq!(empty.status, StatusCode::OK);
        assert_eq!(empty.json()["minecraft"], Value::Null);
        assert_eq!(empty.json(), every.json());
    }
}
