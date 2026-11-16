use std::{collections::BTreeMap, sync::Arc};

use axum::{
    Json,
    extract::{Query, State, rejection::QueryRejection},
};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use utoipa::{IntoParams, ToSchema};

use super::{LIST_FRESH_MS, ListFailure, Origin, document, version_order};
use crate::{app::AppState, error::ApiError};

pub(super) const JAVA_RUNTIMES_PATH: &str =
    "/v1/products/java-runtime/2ec0cc96c44e5a76b9c8b7c39df7210883d12871/all.json";
const SNAPSHOT_SUFFIX: &str = "-snapshot";

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

pub(super) type RuntimeIndex = BTreeMap<String, Box<RawValue>>;

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

pub(super) enum Wanted {
    Major(u32),
    Component(String),
}

pub(super) fn runtime_major(name: &str) -> Option<u32> {
    let digits: String = name.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

fn first_listing(listings: &RawValue) -> Option<RuntimeListing> {
    let listings: Vec<&RawValue> = serde_json::from_str(listings.get()).ok()?;
    serde_json::from_str(listings.first()?.get()).ok()
}

pub(super) fn pick(index: &RuntimeIndex, platform: &str, wanted: &Wanted) -> Option<JavaRuntime> {
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
        (status = 500, description = "The gateway failed to finish the request: `INTERNAL`", body = ApiError),
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
            runtime = %runtime.component,
            url = %runtime.manifest.url,
            "Mojang's list of runtimes points a runtime outside its own host"
        );
        return Err(ListFailure::unreadable(Origin::Mojang).into());
    }
    Ok(Json(runtime))
}
