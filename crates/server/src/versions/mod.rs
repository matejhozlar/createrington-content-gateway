pub mod java;
pub mod loaders;
pub mod minecraft;

use std::{sync::Arc, time::Duration};

use axum::{
    body::Bytes,
    http::{Method, StatusCode},
};
use serde::de::DeserializeOwned;

use crate::{
    app::AppState,
    cache::{Answer, Entry, LoadFailure, Outcome},
    clock::HOUR_MS,
    error::ApiError,
    load,
    upstream::{self, UpstreamFailure},
};

pub const MOJANG_META: &str = "https://piston-meta.mojang.com";
pub const NEOFORGE_MAVEN: &str = "https://maven.neoforged.net";
pub const FORGE_FILES: &str = "https://files.minecraftforge.net";
pub const FABRIC_META: &str = "https://meta.fabricmc.net";
pub const TIMEOUT: Duration = Duration::from_secs(10);
const LIST_FRESH_MS: u64 = HOUR_MS;

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
            tracing::warn!(
                upstream = name,
                %url,
                status = %sent.answer.status,
                "the upstream answered a version list with an unexpected status"
            );
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

/// Why a version list could not be given: what went wrong with its source, or a load that ended without an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ListFailure {
    #[error("{} {failure}", origin.name())]
    Upstream {
        origin: Origin,
        failure: UpstreamFailure,
    },
    #[error("the load of a version list ended without an answer")]
    Lost,
}

impl ListFailure {
    fn unreadable(origin: Origin) -> Self {
        Self::Upstream {
            origin,
            failure: UpstreamFailure::Unreadable,
        }
    }
}

impl From<ListFailure> for ApiError {
    fn from(failed: ListFailure) -> Self {
        match failed {
            ListFailure::Upstream { origin, failure } => ApiError::upstream(failure, origin.name()),
            ListFailure::Lost => ApiError::internal(),
        }
    }
}

fn parsed<T: DeserializeOwned>(body: &[u8], origin: Origin, url: &str) -> Result<T, ListFailure> {
    serde_json::from_slice(body).map_err(|error| {
        tracing::warn!(
            upstream = origin.name(),
            %url,
            %error,
            "the upstream answered a version list with something the gateway cannot read"
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
            if parsed::<T>(&body, origin, &url).is_err() {
                return Outcome::called(Err(LoadFailure::Upstream(UpstreamFailure::Unreadable)));
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
        Err(LoadFailure::Upstream(failure)) => Err(ListFailure::Upstream { origin, failure }),
        Err(LoadFailure::Lost) => Err(ListFailure::Lost),
        Err(LoadFailure::Answered(_) | LoadFailure::Refused(_)) => {
            Err(ListFailure::unreadable(origin))
        }
    }
}

fn version_order(version: &str) -> Vec<u64> {
    version
        .split(['.', '-'])
        .map(|part| part.parse().unwrap_or(0))
        .collect()
}

#[cfg(test)]
mod tests;
