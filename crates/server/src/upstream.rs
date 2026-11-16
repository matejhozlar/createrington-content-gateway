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

    /// The characters a path segment of this source may hold besides letters, digits, `-` and `_`.
    pub fn slug_characters(self) -> &'static str {
        match self {
            Self::CurseForge => "",
            Self::Modrinth => ".+!()~',@$",
        }
    }

    /// What this source's cache and budget keys start with. CurseForge's have none, as the first release stored them.
    pub fn namespace(self) -> &'static str {
        match self {
            Self::CurseForge => "",
            Self::Modrinth => "modrinth",
        }
    }

    /// A cache key of this source, under its namespace.
    pub fn key(self, key: &str) -> String {
        match self.namespace() {
            "" => key.to_owned(),
            namespace => format!("{namespace}:{key}"),
        }
    }
}

/// Why an upstream gave no answer, worded to follow the upstream's name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum UpstreamFailure {
    #[error("did not answer in time")]
    Timeout,
    #[error("could not be reached")]
    Unreachable,
    #[error("answered with more than the gateway keeps")]
    TooLarge,
    #[error("answered with something the gateway cannot read")]
    Unreadable,
}

/// What an upstream answered, with its `Retry-After` or `X-Ratelimit-Reset`.
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
    if let Some(bytes) = announced.filter(|length| *length > MAX_ANSWER_BYTES) {
        return Err(too_large(name, bytes));
    }
    let mut body = Vec::with_capacity(announced.unwrap_or(0));
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| failure(error, name))?
    {
        if body.len() + chunk.len() > MAX_ANSWER_BYTES {
            return Err(too_large(name, body.len() + chunk.len()));
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

fn too_large(name: &'static str, bytes: usize) -> UpstreamFailure {
    tracing::warn!(
        upstream = name,
        bytes,
        limit = MAX_ANSWER_BYTES,
        "the upstream's answer is larger than the gateway reads"
    );
    UpstreamFailure::TooLarge
}

/// Why `name` gave no answer, from the error reqwest reported.
pub fn failure(error: reqwest::Error, name: &'static str) -> UpstreamFailure {
    if error.is_timeout() {
        tracing::warn!(upstream = name, %error, "the upstream did not answer in time");
        UpstreamFailure::Timeout
    } else {
        tracing::warn!(upstream = name, %error, "the upstream could not be reached");
        UpstreamFailure::Unreachable
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(length: Option<&str>, body: Vec<u8>) -> reqwest::Response {
        let mut response = axum::http::Response::builder().status(200);
        if let Some(length) = length {
            response = response.header(CONTENT_LENGTH, length);
        }
        reqwest::Response::from(response.body(body).unwrap())
    }

    #[tokio::test]
    async fn an_answer_announced_larger_than_the_gateway_keeps_is_not_read() {
        let announced = (MAX_ANSWER_BYTES + 1).to_string();
        let got = read(response(Some(&announced), Vec::new()), "CurseForge").await;
        assert_eq!(got.err(), Some(UpstreamFailure::TooLarge));
    }

    #[tokio::test]
    async fn an_answer_that_grows_past_what_the_gateway_keeps_is_dropped() {
        let got = read(response(None, vec![b'x'; MAX_ANSWER_BYTES + 1]), "Modrinth").await;
        assert_eq!(got.err(), Some(UpstreamFailure::TooLarge));

        let kept = read(response(None, vec![b'x'; MAX_ANSWER_BYTES]), "Modrinth").await;
        assert_eq!(kept.unwrap().answer.body.len(), MAX_ANSWER_BYTES);
    }
}
