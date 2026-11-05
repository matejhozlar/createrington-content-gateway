use std::time::Duration;

use axum::{
    body::Bytes,
    http::{
        Method,
        header::{ACCEPT, CONTENT_LENGTH, CONTENT_TYPE, RETRY_AFTER},
    },
};

use crate::{cache::Answer, config::ApiKey};

const MAX_ANSWER_BYTES: usize = 32 * 1024 * 1024;
const RATELIMIT_RESET: &str = "x-ratelimit-reset";

/// The APIs the gateway fronts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Source {
    CurseForge,
    Modrinth,
}

impl Source {
    /// The name in log lines and error messages.
    pub fn name(self) -> &'static str {
        match self {
            Self::CurseForge => "CurseForge",
            Self::Modrinth => "Modrinth",
        }
    }

    /// What a cache key of this source starts with. CurseForge's keys have no prefix, as the first release stored them.
    pub fn key(self, key: &str) -> String {
        match self {
            Self::CurseForge => key.to_owned(),
            Self::Modrinth => format!("modrinth:{key}"),
        }
    }
}

/// Why an upstream gave no answer: it took longer than the timeout, could not be reached, sent more than the gateway
/// keeps, or sent a batch answer the gateway could not split into rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamFailure {
    Timeout,
    Unreachable,
    TooLarge,
    Unreadable,
}

/// What an upstream answered, with the headers that say when to try again: `Retry-After`, or Modrinth's
/// `X-Ratelimit-Reset` in seconds when that is all there is.
pub struct Sent {
    pub answer: Answer,
    pub retry_after: Option<String>,
}

/// The client that calls one upstream API, with its key when it needs one.
pub struct Upstream {
    client: reqwest::Client,
    base: String,
    key: Option<ApiKey>,
    source: Source,
}

impl Upstream {
    /// `base` includes the API version, such as `https://api.curseforge.com/v1`.
    pub fn new(
        source: Source,
        base: impl Into<String>,
        key: Option<ApiKey>,
        user_agent: &str,
        timeout: Duration,
    ) -> reqwest::Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(timeout)
                .user_agent(user_agent)
                .build()?,
            base: base.into(),
            key,
            source,
        })
    }

    pub async fn send(
        &self,
        method: Method,
        path: &str,
        query: Option<&str>,
        body: Bytes,
    ) -> Result<Sent, UpstreamFailure> {
        let mut url = format!("{}/{path}", self.base);
        if let Some(query) = query.filter(|query| !query.is_empty()) {
            url.push('?');
            url.push_str(query);
        }
        let mut request = self
            .client
            .request(method, url)
            .header(ACCEPT, "application/json");
        if let Some(key) = &self.key {
            request = request.header("x-api-key", key.as_str());
        }
        if !body.is_empty() {
            request = request.header(CONTENT_TYPE, "application/json").body(body);
        }
        let response = request
            .send()
            .await
            .map_err(|error| failure(error, self.source.name()))?;
        read(response, self.source.name()).await
    }
}

/// An answer read up to the size the gateway keeps, with its `Retry-After` or `X-Ratelimit-Reset`.
pub async fn read(
    mut response: reqwest::Response,
    name: &'static str,
) -> Result<Sent, UpstreamFailure> {
    let status = response.status();
    let content_type = response.headers().get(CONTENT_TYPE).cloned();
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    let retry_after = header(RETRY_AFTER.as_str()).or_else(|| header(RATELIMIT_RESET));
    let announced = response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok()?.parse::<usize>().ok());
    if announced.is_some_and(|length| length > MAX_ANSWER_BYTES) {
        return Err(UpstreamFailure::TooLarge);
    }
    let mut body = Vec::with_capacity(announced.unwrap_or(0));
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| failure(error, name))?
    {
        if body.len() + chunk.len() > MAX_ANSWER_BYTES {
            return Err(UpstreamFailure::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Sent {
        answer: Answer {
            status,
            content_type,
            body: body.into(),
        },
        retry_after,
    })
}

/// Why `name` gave no answer, from the error reqwest reported.
pub fn failure(error: reqwest::Error, name: &'static str) -> UpstreamFailure {
    if error.is_timeout() {
        tracing::warn!("{name} did not answer in time: {error}");
        UpstreamFailure::Timeout
    } else {
        tracing::warn!("{name} could not be reached: {error}");
        UpstreamFailure::Unreachable
    }
}
