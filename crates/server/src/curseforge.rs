use std::{sync::Arc, time::Duration};

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{
        HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri,
        header::{ACCEPT, CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE, RETRY_AFTER},
    },
    response::{IntoResponse, Response},
};

use crate::{
    app::AppState,
    auth::Pool,
    batch::{self, Batch},
    budget::{self, Refusal},
    cache::{Answer, AnswerCache, Entry, Finisher, LoadFailure, Outcome},
    config::ApiKey,
    error::ApiError,
};

pub const CURSEFORGE_API: &str = "https://api.curseforge.com";
pub const TIMEOUT: Duration = Duration::from_secs(10);
const MAX_ANSWER_BYTES: usize = 32 * 1024 * 1024;
pub const MINUTE_MS: u64 = 60_000;
pub const HOUR_MS: u64 = 60 * MINUTE_MS;
const DAY_MS: u64 = 24 * HOUR_MS;
const X_CACHE: HeaderName = HeaderName::from_static("x-cache");

/// Why CurseForge gave no answer: it took longer than the timeout, could not be reached, sent more than the gateway
/// keeps, or sent a batch answer the gateway could not split into rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamFailure {
    Timeout,
    Unreachable,
    TooLarge,
    Unreadable,
}

/// The client that calls the CurseForge API with the gateway's key.
pub struct Upstream {
    client: reqwest::Client,
    base: String,
    key: ApiKey,
}

impl Upstream {
    pub fn new(base: impl Into<String>, key: ApiKey, timeout: Duration) -> reqwest::Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(timeout)
                .user_agent(concat!("content-gateway/", env!("CARGO_PKG_VERSION")))
                .build()?,
            base: base.into(),
            key,
        })
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        query: Option<&str>,
        body: Bytes,
    ) -> Result<(Answer, Option<String>), UpstreamFailure> {
        let mut url = format!("{}/v1/{path}", self.base);
        if let Some(query) = query.filter(|query| !query.is_empty()) {
            url.push('?');
            url.push_str(query);
        }
        let mut request = self
            .client
            .request(method, url)
            .header("x-api-key", self.key.as_str())
            .header(ACCEPT, "application/json");
        if !body.is_empty() {
            request = request.header(CONTENT_TYPE, "application/json").body(body);
        }

        let mut response = request.send().await.map_err(failure)?;
        let status = response.status();
        let content_type = response.headers().get(CONTENT_TYPE).cloned();
        let retry_after = response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let announced = response
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok()?.parse::<usize>().ok());
        if announced.is_some_and(|length| length > MAX_ANSWER_BYTES) {
            return Err(UpstreamFailure::TooLarge);
        }
        let mut body = Vec::with_capacity(announced.unwrap_or(0));
        while let Some(chunk) = response.chunk().await.map_err(failure)? {
            if body.len() + chunk.len() > MAX_ANSWER_BYTES {
                return Err(UpstreamFailure::TooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        Ok((
            Answer {
                status,
                content_type,
                body: body.into(),
            },
            retry_after,
        ))
    }
}

fn failure(error: reqwest::Error) -> UpstreamFailure {
    if error.is_timeout() {
        tracing::warn!("CurseForge did not answer in time: {error}");
        UpstreamFailure::Timeout
    } else {
        tracing::warn!("CurseForge could not be reached: {error}");
        UpstreamFailure::Unreachable
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Freshness {
    found_ms: u64,
    not_found_ms: Option<u64>,
}

const fn fresh(found_ms: u64, not_found_ms: Option<u64>) -> Option<Freshness> {
    Some(Freshness {
        found_ms,
        not_found_ms,
    })
}

fn freshness(segments: &[&str]) -> Option<Freshness> {
    let id = |segment: &str| !segment.is_empty() && segment.bytes().all(|b| b.is_ascii_digit());
    match segments {
        ["mods", "search"] => fresh(5 * MINUTE_MS, None),
        ["categories"] | ["minecraft", "version"] | ["minecraft", "modloader"] => {
            fresh(DAY_MS, None)
        }
        ["mods", project] if id(project) => {
            fresh(Batch::Mods.found_ms(), Some(Batch::Mods.not_found_ms()))
        }
        ["mods", project, "files"] if id(project) => fresh(10 * MINUTE_MS, None),
        ["mods", project, "description"] if id(project) => fresh(DAY_MS, None),
        ["mods", project, "files", file] if id(project) && id(file) => {
            fresh(Batch::Files.found_ms(), Some(Batch::Files.not_found_ms()))
        }
        ["mods", project, "files", file, "download-url"] if id(project) && id(file) => {
            fresh(HOUR_MS, Some(10 * MINUTE_MS))
        }
        ["mods", project, "files", file, "changelog"] if id(project) && id(file) => {
            fresh(DAY_MS, None)
        }
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Share {
    Mod(u64),
    File { project: u64, file: u64 },
}

fn share_of(segments: &[&str], query: Option<&str>) -> Option<Share> {
    if query.is_some_and(|query| !query.is_empty()) {
        return None;
    }
    match segments {
        ["mods", project] => project.parse().ok().map(Share::Mod),
        ["mods", project, "files", file] => Some(Share::File {
            project: project.parse().ok()?,
            file: file.parse().ok()?,
        }),
        _ => None,
    }
}

fn valid_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn cache_key(path: &str, query: Option<&str>) -> String {
    let mut pairs: Vec<&str> = query
        .unwrap_or_default()
        .split('&')
        .filter(|pair| !pair.is_empty())
        .collect();
    pairs.sort_unstable();
    format!("{path}?{}", pairs.join("&"))
}

fn wants_fresh(headers: &HeaderMap) -> bool {
    headers.get_all(CACHE_CONTROL).iter().any(|value| {
        value.to_str().is_ok_and(|value| {
            value
                .split(',')
                .any(|directive| directive.trim().eq_ignore_ascii_case("no-cache"))
        })
    })
}

/// What `GET api.curseforge.com/v1/{path}` answers, from the cache while it is fresh.
///
/// The query string goes to CurseForge as it came, and the answer comes back with CurseForge's status, content type
/// and body, marked `X-Cache`. The routes of the cache table are kept for their fresh time; every other route is
/// asked each time. `Cache-Control: no-cache` asks CurseForge again and keeps the new answer. A kept answer that is
/// no longer fresh is still given, marked `stale`, when the budget refuses the call or CurseForge fails.
#[utoipa::path(
    get,
    path = "/v1/curseforge/{path}",
    tag = "curseforge",
    security(("bearer" = [])),
    params(
        ("path" = String, Path, description = "The path under `api.curseforge.com/v1/`, such as `mods/search` or `mods/238222/files`: segments of letters, digits, `-` and `_`", example = "mods/search"),
        ("X-Budget-Pool" = Option<String>, Header, description = "The pool the call draws from. Required when the token has more than one pool"),
        ("Cache-Control" = Option<String>, Header, description = "`no-cache` asks CurseForge again instead of answering from the cache"),
    ),
    responses(
        (status = 200, description = "CurseForge's answer, as it came", body = Value, headers(("X-Cache" = String, description = "`hit` from the cache, `miss` from CurseForge, `stale` an old answer given because CurseForge could not be asked"))),
        (status = 400, description = "`INVALID_PATH`, `POOL_REQUIRED` or `UNKNOWN_POOL`", body = ApiError),
        (status = 401, description = "No valid consumer token", body = ApiError),
        (status = 502, description = "CurseForge could not be reached or answered something the gateway cannot keep: `UPSTREAM_UNREACHABLE`, `UPSTREAM_TOO_LARGE` or `UPSTREAM_UNREADABLE`", body = ApiError),
        (status = 503, description = "The call was refused: `BUDGET_SPENT` this minute or `UPSTREAM_HOLD` after a 429 from CurseForge", body = ApiError, headers(("Retry-After" = u64, description = "Seconds until the next call can go out"))),
        (status = 504, description = "CurseForge did not answer in time, `UPSTREAM_TIMEOUT`", body = ApiError),
        (status = "default", description = "Any other status CurseForge answered, with its body. Only the gateway's own answers lack `X-Cache`"),
    )
)]
pub async fn passthrough_get(
    State(state): State<Arc<AppState>>,
    Path(path): Path<String>,
    pool: Pool,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    passthrough(state, path, pool, Method::GET, uri, headers, Bytes::new()).await
}

/// What `POST api.curseforge.com/v1/{path}` answers to this body.
///
/// `POST mods` with only `{"modIds": [...]}`, `POST mods/files` with only `{"fileIds": [...]}`, and
/// `POST fingerprints/432` or `POST fingerprints` with only `{"fingerprints": [...]}` are answered per id from the
/// cache, and only the ids not kept are asked for, 100 per call. Every other body goes to CurseForge each time.
#[utoipa::path(
    post,
    path = "/v1/curseforge/{path}",
    tag = "curseforge",
    security(("bearer" = [])),
    params(
        ("path" = String, Path, description = "The path under `api.curseforge.com/v1/`, such as `mods` or `fingerprints/432`: segments of letters, digits, `-` and `_`", example = "mods"),
        ("X-Budget-Pool" = Option<String>, Header, description = "The pool the call draws from. Required when the token has more than one pool"),
        ("Cache-Control" = Option<String>, Header, description = "`no-cache` asks CurseForge again instead of answering from the cache"),
    ),
    request_body(content = Value, description = "The JSON body CurseForge expects at this path", content_type = "application/json"),
    responses(
        (status = 200, description = "CurseForge's answer, as it came, or one built from the kept rows of a batch", body = Value, headers(("X-Cache" = String, description = "`hit` when every id was kept, `miss` when CurseForge was asked, `stale` an old answer given because CurseForge could not be asked"))),
        (status = 400, description = "`INVALID_PATH`, `POOL_REQUIRED` or `UNKNOWN_POOL`", body = ApiError),
        (status = 401, description = "No valid consumer token", body = ApiError),
        (status = 413, description = "The batch needs more calls than a whole minute allows, `REQUEST_TOO_LARGE`; split it", body = ApiError),
        (status = 502, description = "CurseForge could not be reached or answered something the gateway cannot keep: `UPSTREAM_UNREACHABLE`, `UPSTREAM_TOO_LARGE` or `UPSTREAM_UNREADABLE`", body = ApiError),
        (status = 503, description = "The call was refused: `BUDGET_SPENT` this minute or `UPSTREAM_HOLD` after a 429 from CurseForge", body = ApiError, headers(("Retry-After" = u64, description = "Seconds until the next call can go out"))),
        (status = 504, description = "CurseForge did not answer in time, `UPSTREAM_TIMEOUT`", body = ApiError),
        (status = "default", description = "Any other status CurseForge answered, with its body. Only the gateway's own answers lack `X-Cache`"),
    )
)]
pub async fn passthrough_post(
    State(state): State<Arc<AppState>>,
    Path(path): Path<String>,
    pool: Pool,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    passthrough(state, path, pool, Method::POST, uri, headers, body).await
}

async fn passthrough(
    state: Arc<AppState>,
    path: String,
    pool: Pool,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let segments: Vec<&str> = path.split('/').collect();
    if !segments.iter().all(|segment| valid_segment(segment)) {
        return ApiError::invalid_path().into_response();
    }
    let query = uri.query().map(str::to_owned);
    let forced = wants_fresh(&headers);

    if method == Method::POST
        && query.as_deref().is_none_or(str::is_empty)
        && let Some(batch) = Batch::of(&segments)
        && let Some(ids) = batch.ids(&body)
    {
        return batch::answer(state, pool, batch, ids, forced).await;
    }

    let Some(freshness) = (method == Method::GET)
        .then(|| freshness(&segments))
        .flatten()
    else {
        let call = {
            let (state, pool) = (state.clone(), pool.clone());
            tokio::spawn(
                async move { call(&state, &pool, method, &path, query.as_deref(), body).await },
            )
        };
        return match call.await {
            Ok(Ok(answer)) => respond(&answer, "miss"),
            Ok(Err(failure)) => failed(failure, &pool),
            Err(_) => failed(LoadFailure::Upstream(UpstreamFailure::Unreachable), &pool),
        };
    };

    let share = share_of(&segments, query.as_deref());
    let key = match share {
        Some(Share::Mod(project)) => Batch::Mods.key(project),
        _ => cache_key(&path, query.as_deref()),
    };
    let (shared_file, cached) = match share {
        Some(Share::File { file, .. }) => {
            let mut found = state
                .cache
                .lookup_many(&[Batch::Files.key(file), key.clone()], state.clock.now_ms())
                .await;
            let cached = found.pop().flatten();
            (found.pop().flatten(), cached)
        }
        _ => (None, state.cache.lookup(&key, state.clock.now_ms()).await),
    };
    let now = state.clock.now_ms();
    if !forced
        && let Some(Share::File { project, .. }) = share
        && let Some(entry) = shared_file.filter(|entry| {
            entry.is_fresh(now)
                && entry.answer.status == StatusCode::OK
                && entry.owner == Some(project)
        })
    {
        state.budget.record_hit(&pool.consumer, &pool.name);
        return respond(&entry.answer, "hit");
    }
    if !forced && let Some(entry) = cached.as_ref().filter(|entry| entry.is_fresh(now)) {
        state.budget.record_hit(&pool.consumer, &pool.name);
        return respond(&entry.answer, "hit");
    }

    let load_key = if forced {
        format!("{key} no-cache")
    } else {
        key.clone()
    };
    let (waiting, finisher) = state.cache.join(&load_key);
    let started = finisher.is_some();
    if let Some(finisher) = finisher {
        let load = Load {
            state: state.clone(),
            pool: pool.clone(),
            key,
            load_key,
            path,
            query,
            freshness,
            share,
            forced,
        };
        tokio::spawn(load.run(finisher));
    }
    let outcome = AnswerCache::wait(waiting).await.unwrap_or(Outcome {
        loaded: Err(LoadFailure::Upstream(UpstreamFailure::Unreachable)),
        called: false,
    });

    match outcome.loaded {
        Ok(entry) => {
            if started && outcome.called {
                respond(&entry.answer, "miss")
            } else {
                state.budget.record_hit(&pool.consumer, &pool.name);
                respond(&entry.answer, "hit")
            }
        }
        Err(failure) => {
            let curseforge_failed = match &failure {
                LoadFailure::Answered(answer) => answer.status.is_server_error(),
                LoadFailure::Refused(_) | LoadFailure::Upstream(_) => true,
            };
            let stale = cached.filter(|entry| entry.is_stale(state.clock.now_ms()));
            match stale {
                Some(entry) if curseforge_failed => {
                    state.budget.record_hit(&pool.consumer, &pool.name);
                    respond(&entry.answer, "stale")
                }
                _ => failed(failure, &pool),
            }
        }
    }
}

struct Load {
    state: Arc<AppState>,
    pool: Pool,
    key: String,
    load_key: String,
    path: String,
    query: Option<String>,
    freshness: Freshness,
    share: Option<Share>,
    forced: bool,
}

impl Load {
    async fn run(self, finisher: Finisher) {
        let outcome = self.outcome().await;
        self.state.cache.finish(&self.load_key, finisher, outcome);
    }

    async fn outcome(&self) -> Outcome {
        let state = &self.state;
        if !self.forced
            && let Some(entry) = state
                .cache
                .get(&self.key)
                .filter(|entry| entry.is_fresh(state.clock.now_ms()))
        {
            return Outcome {
                loaded: Ok(entry),
                called: false,
            };
        }
        let answer = match call(
            state,
            &self.pool,
            Method::GET,
            &self.path,
            self.query.as_deref(),
            Bytes::new(),
        )
        .await
        {
            Ok(answer) => answer,
            Err(failure) => {
                return Outcome {
                    loaded: Err(failure),
                    called: true,
                };
            }
        };
        let fresh_ms = match answer.status {
            StatusCode::OK => Some(self.freshness.found_ms),
            StatusCode::NOT_FOUND => self.freshness.not_found_ms,
            _ => None,
        };
        let Some(fresh_ms) = fresh_ms else {
            return Outcome {
                loaded: Err(LoadFailure::Answered(answer)),
                called: true,
            };
        };
        let found = answer.status == StatusCode::OK;
        let mut entry = Entry::new(answer, state.clock.now_ms(), fresh_ms);
        if found
            && self.share.is_some()
            && let Some((row, owner)) = batch::row_of(&entry.answer.body)
        {
            entry.row = Some(row);
            entry.owner = owner;
        }
        let entry = Arc::new(entry);
        state.cache.insert(&self.key, entry.clone());
        if let Some(Share::File { project, file }) = self.share
            && entry.row.is_some()
            && entry.owner == Some(project)
        {
            state.cache.insert(&Batch::Files.key(file), entry.clone());
        }
        Outcome {
            loaded: Ok(entry),
            called: true,
        }
    }
}

async fn call(
    state: &AppState,
    pool: &Pool,
    method: Method,
    path: &str,
    query: Option<&str>,
    body: Bytes,
) -> Result<Answer, LoadFailure> {
    state
        .budget
        .admit(&pool.consumer, &pool.name, 1)
        .await
        .map_err(LoadFailure::Refused)?;
    send_recorded(state, pool, method, path, query, body).await
}

/// Sends one call the budget already admitted and records it. A 429 comes back as the hold it starts.
pub async fn send_recorded(
    state: &AppState,
    pool: &Pool,
    method: Method,
    path: &str,
    query: Option<&str>,
    body: Bytes,
) -> Result<Answer, LoadFailure> {
    match state.upstream.send(method, path, query, body).await {
        Ok((answer, retry_after)) => {
            state
                .budget
                .record(
                    &pool.consumer,
                    &pool.name,
                    Some(answer.status.as_u16()),
                    retry_after.as_deref(),
                )
                .await;
            if answer.status == StatusCode::TOO_MANY_REQUESTS {
                return Err(LoadFailure::Refused(Refusal::Hold {
                    retry_after_ms: budget::hold_ms(retry_after.as_deref(), state.clock.now_ms()),
                }));
            }
            Ok(answer)
        }
        Err(failure) => {
            state
                .budget
                .record(&pool.consumer, &pool.name, None, None)
                .await;
            Err(LoadFailure::Upstream(failure))
        }
    }
}

pub fn respond(answer: &Answer, cache: &'static str) -> Response {
    let mut response = (answer.status, answer.body.clone()).into_response();
    let headers = response.headers_mut();
    match &answer.content_type {
        Some(content_type) => {
            headers.insert(CONTENT_TYPE, content_type.clone());
        }
        None => {
            headers.remove(CONTENT_TYPE);
        }
    }
    headers.insert(X_CACHE, HeaderValue::from_static(cache));
    response
}

pub fn failed(failure: LoadFailure, pool: &Pool) -> Response {
    match failure {
        LoadFailure::Answered(answer) => respond(&answer, "miss"),
        LoadFailure::Refused(refusal) => ApiError::refused(refusal, &pool.name).into_response(),
        LoadFailure::Upstream(failure) => ApiError::upstream(failure).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    use super::*;
    use crate::test_support::*;

    #[tokio::test]
    async fn passes_an_answer_through_as_it_came() {
        let mock = Mock::start(Duration::ZERO, |_, _| {
            reply(
                200,
                r#"{ "data" : [ {"id":1,"name":"JEI"} ], "pagination": {} }"#,
            )
        })
        .await;
        let gateway = gateway(&mock, 300, TIMEOUT);

        let got = get(
            &gateway.router,
            "/v1/curseforge/mods/search?gameId=432&searchFilter=jei%20mod",
        )
        .await;
        assert_eq!(got.status, StatusCode::OK);
        assert_eq!(
            got.body,
            r#"{ "data" : [ {"id":1,"name":"JEI"} ], "pagination": {} }"#
        );
        assert_eq!(
            got.header("content-type"),
            Some("application/json; charset=utf-8")
        );
        assert_eq!(got.header("x-cache"), Some("miss"));

        let seen = mock.seen();
        assert_eq!(seen[0].method, "GET");
        assert_eq!(
            seen[0].uri,
            "/v1/mods/search?gameId=432&searchFilter=jei%20mod"
        );
        assert_eq!(seen[0].key.as_deref(), Some(KEY));
    }

    #[tokio::test]
    async fn a_fresh_answer_comes_from_the_cache_and_counts_as_a_hit() {
        let mock = Mock::start(Duration::ZERO, |_, _| reply(200, r#"{"data":{"id":1}}"#)).await;
        let gateway = gateway(&mock, 300, TIMEOUT);

        assert_eq!(
            get(&gateway.router, "/v1/curseforge/mods/1")
                .await
                .header("x-cache"),
            Some("miss")
        );
        let again = get(&gateway.router, "/v1/curseforge/mods/1").await;
        assert_eq!(again.header("x-cache"), Some("hit"));
        assert_eq!(again.body, r#"{"data":{"id":1}}"#);
        assert_eq!(mock.calls(), 1);

        let snapshot = gateway.state.budget.snapshot().await;
        assert_eq!(snapshot.used, 1);
        assert_eq!(snapshot.days[0].pools[0].counts.calls, 1);
        assert_eq!(snapshot.days[0].pools[0].counts.hits, 1);
    }

    #[tokio::test]
    async fn the_order_of_query_parameters_does_not_matter_to_the_cache() {
        let mock = Mock::start(Duration::ZERO, |_, _| reply(200, "{}")).await;
        let gateway = gateway(&mock, 300, TIMEOUT);
        get(
            &gateway.router,
            "/v1/curseforge/mods/search?gameId=432&classId=6",
        )
        .await;
        let again = get(
            &gateway.router,
            "/v1/curseforge/mods/search?classId=6&gameId=432",
        )
        .await;
        assert_eq!(again.header("x-cache"), Some("hit"));
        get(
            &gateway.router,
            "/v1/curseforge/mods/search?gameId=432&classId=12",
        )
        .await;
        assert_eq!(mock.calls(), 2);
    }

    #[tokio::test]
    async fn an_answer_stays_fresh_for_the_time_of_its_route() {
        let mock = Mock::start(Duration::ZERO, |_, _| reply(200, "{}")).await;
        let gateway = gateway(&mock, 300, TIMEOUT);
        get(&gateway.router, "/v1/curseforge/mods/1").await;
        get(&gateway.router, "/v1/curseforge/mods/search?gameId=432").await;

        gateway.clock.advance(6 * MINUTE_MS);
        assert_eq!(
            get(&gateway.router, "/v1/curseforge/mods/1")
                .await
                .header("x-cache"),
            Some("hit")
        );
        assert_eq!(
            get(&gateway.router, "/v1/curseforge/mods/search?gameId=432")
                .await
                .header("x-cache"),
            Some("miss")
        );

        gateway.clock.advance(55 * MINUTE_MS);
        assert_eq!(
            get(&gateway.router, "/v1/curseforge/mods/1")
                .await
                .header("x-cache"),
            Some("miss")
        );
        assert_eq!(mock.calls(), 4);
    }

    #[tokio::test]
    async fn not_found_is_kept_for_the_shorter_not_found_time() {
        let mock = Mock::start(Duration::ZERO, |_, _| reply(404, "")).await;
        let gateway = gateway(&mock, 300, TIMEOUT);
        assert_eq!(
            get(&gateway.router, "/v1/curseforge/mods/1").await.status,
            StatusCode::NOT_FOUND
        );
        gateway.clock.advance(4 * MINUTE_MS);
        let again = get(&gateway.router, "/v1/curseforge/mods/1").await;
        assert_eq!(
            (again.status, again.header("x-cache")),
            (StatusCode::NOT_FOUND, Some("hit"))
        );
        gateway.clock.advance(2 * MINUTE_MS);
        assert_eq!(
            get(&gateway.router, "/v1/curseforge/mods/1")
                .await
                .header("x-cache"),
            Some("miss")
        );
        assert_eq!(mock.calls(), 2);
    }

    #[tokio::test]
    async fn no_cache_asks_curseforge_again_and_keeps_the_new_answer() {
        let mock = Mock::start(Duration::ZERO, |call, _| {
            reply(200, format!("{{\"call\":{call}}}"))
        })
        .await;
        let gateway = gateway(&mock, 300, TIMEOUT);
        get(&gateway.router, "/v1/curseforge/mods/1/files").await;

        let forced = send(
            &gateway.router,
            Request::get("/v1/curseforge/mods/1/files")
                .header("cache-control", "max-age=0, no-cache"),
            "",
        )
        .await;
        assert_eq!(forced.header("x-cache"), Some("miss"));
        assert_eq!(forced.body, r#"{"call":1}"#);

        let after = get(&gateway.router, "/v1/curseforge/mods/1/files").await;
        assert_eq!(after.header("x-cache"), Some("hit"));
        assert_eq!(after.body, r#"{"call":1}"#);
        assert_eq!(mock.calls(), 2);
    }

    #[tokio::test]
    async fn concurrent_misses_make_one_call() {
        let mock = Mock::start(Duration::from_millis(100), |_, _| reply(200, "{}")).await;
        let gateway = gateway(&mock, 300, TIMEOUT);
        let requests: Vec<_> = (0..5)
            .map(|_| {
                let router = gateway.router.clone();
                tokio::spawn(async move { get(&router, "/v1/curseforge/mods/1").await })
            })
            .collect();
        let mut caches = Vec::new();
        for request in requests {
            caches.push(request.await.unwrap().header("x-cache").unwrap().to_owned());
        }
        caches.sort();
        assert_eq!(caches, ["hit", "hit", "hit", "hit", "miss"]);
        assert_eq!(mock.calls(), 1);
        assert_eq!(
            gateway.state.budget.snapshot().await.days[0].pools[0]
                .counts
                .hits,
            4
        );
    }

    #[tokio::test]
    async fn a_call_finishes_and_is_recorded_after_its_caller_is_gone() {
        let mock = Mock::start(Duration::from_millis(200), |_, _| reply(200, "{}")).await;
        let gateway = gateway(&mock, 300, TIMEOUT);
        let caller = {
            let router = gateway.router.clone();
            tokio::spawn(async move { get(&router, "/v1/curseforge/mods/1").await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        caller.abort();
        tokio::time::sleep(Duration::from_millis(400)).await;

        assert_eq!(mock.calls(), 1);
        assert_eq!(gateway.state.cache.loads_running(), 0);
        assert_eq!(
            gateway.state.budget.snapshot().await.days[0].pools[0]
                .counts
                .calls,
            1
        );
        assert_eq!(
            get(&gateway.router, "/v1/curseforge/mods/1")
                .await
                .header("x-cache"),
            Some("hit")
        );
    }

    #[tokio::test]
    async fn no_cache_does_not_join_a_load_that_may_not_ask_curseforge() {
        let mock = Mock::start(Duration::from_millis(200), |call, _| {
            reply(200, format!("{{\"call\":{call}}}"))
        })
        .await;
        let gateway = gateway(&mock, 300, TIMEOUT);
        let plain = {
            let router = gateway.router.clone();
            tokio::spawn(async move { get(&router, "/v1/curseforge/mods/1").await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        let forced = send(
            &gateway.router,
            Request::get("/v1/curseforge/mods/1").header("cache-control", "no-cache"),
            "",
        )
        .await;
        plain.await.unwrap();

        assert_eq!(forced.header("x-cache"), Some("miss"));
        assert_eq!(forced.body, r#"{"call":1}"#);
        assert_eq!(mock.calls(), 2);
    }

    #[tokio::test]
    async fn a_429_holds_every_route_off() {
        let mock = Mock::start(Duration::ZERO, |_, _| Reply {
            status: 429,
            headers: vec![("retry-after", "30")],
            body: String::new(),
        })
        .await;
        let gateway = gateway(&mock, 300, TIMEOUT);

        let throttled = get(&gateway.router, "/v1/curseforge/mods/1").await;
        assert_eq!(throttled.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(throttled.header("retry-after"), Some("30"));
        assert_eq!(throttled.json()["code"], "UPSTREAM_HOLD");

        let held = get(&gateway.router, "/v1/curseforge/categories?gameId=432").await;
        assert_eq!(held.json()["code"], "UPSTREAM_HOLD");
        assert_eq!(held.json()["pool"], "launcher");
        assert_eq!(mock.calls(), 1);
        assert_eq!(
            gateway.state.budget.snapshot().await.days[0].pools[0]
                .counts
                .throttled,
            1
        );
    }

    #[tokio::test]
    async fn a_stale_answer_is_served_when_curseforge_fails() {
        let mock = Mock::start(Duration::ZERO, |call, _| match call {
            0 => reply(200, r#"{"data":"old"}"#),
            _ => reply(502, "bad gateway"),
        })
        .await;
        let gateway = gateway(&mock, 300, TIMEOUT);
        get(&gateway.router, "/v1/curseforge/mods/1").await;
        gateway.clock.advance(2 * HOUR_MS);

        let stale = get(&gateway.router, "/v1/curseforge/mods/1").await;
        assert_eq!(stale.status, StatusCode::OK);
        assert_eq!(stale.header("x-cache"), Some("stale"));
        assert_eq!(stale.body, r#"{"data":"old"}"#);

        gateway.clock.advance(DAY_MS);
        let gone = get(&gateway.router, "/v1/curseforge/mods/1").await;
        assert_eq!(
            (gone.status, gone.body),
            (StatusCode::BAD_GATEWAY, Bytes::from("bad gateway"))
        );
    }

    #[tokio::test]
    async fn a_stale_answer_is_served_when_the_budget_refuses() {
        let mock = Mock::start(Duration::ZERO, |_, _| reply(200, "{}")).await;
        let gateway = gateway(&mock, 1, TIMEOUT);
        get(&gateway.router, "/v1/curseforge/mods/1").await;
        gateway.clock.advance(2 * HOUR_MS);
        get(&gateway.router, "/v1/curseforge/mods/2").await;

        let stale = get(&gateway.router, "/v1/curseforge/mods/1").await;
        assert_eq!(stale.header("x-cache"), Some("stale"));
        let refused = get(&gateway.router, "/v1/curseforge/mods/3").await;
        assert_eq!(refused.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(refused.json()["code"], "BUDGET_SPENT");
        assert_eq!(mock.calls(), 2);
    }

    #[tokio::test]
    async fn an_error_answer_is_passed_on_and_not_kept() {
        let mock = Mock::start(Duration::ZERO, |_, _| reply(500, r#"{"error":"boom"}"#)).await;
        let gateway = gateway(&mock, 300, TIMEOUT);
        let first = get(&gateway.router, "/v1/curseforge/mods/1").await;
        assert_eq!(
            (first.status, first.body),
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Bytes::from(r#"{"error":"boom"}"#)
            )
        );
        get(&gateway.router, "/v1/curseforge/mods/1").await;
        assert_eq!(mock.calls(), 2);
    }

    #[tokio::test]
    async fn a_post_that_is_not_a_plain_batch_goes_through_with_its_body() {
        let mock = Mock::start(Duration::ZERO, |_, _| reply(200, r#"{"data":[]}"#)).await;
        let gateway = gateway(&mock, 300, TIMEOUT);
        for _ in 0..2 {
            let got = send(
                &gateway.router,
                Request::post("/v1/curseforge/mods"),
                r#"{"modIds":[1,2],"filterPcOnly":true}"#,
            )
            .await;
            assert_eq!(got.body, r#"{"data":[]}"#);
        }
        let seen = mock.seen();
        assert_eq!(
            (seen[0].method.as_str(), seen[0].uri.as_str()),
            ("POST", "/v1/mods")
        );
        assert_eq!(seen[0].body, r#"{"modIds":[1,2],"filterPcOnly":true}"#);
        assert_eq!(gateway.state.budget.snapshot().await.used, 2);
    }

    #[tokio::test]
    async fn a_route_outside_the_cache_table_is_counted_but_not_kept() {
        let mock = Mock::start(Duration::ZERO, |_, _| reply(200, "{}")).await;
        let gateway = gateway(&mock, 300, TIMEOUT);
        get(&gateway.router, "/v1/curseforge/games").await;
        get(&gateway.router, "/v1/curseforge/games").await;
        assert_eq!(mock.calls(), 2);
        assert_eq!(gateway.state.budget.snapshot().await.used, 2);
    }

    #[tokio::test]
    async fn refuses_a_path_that_could_leave_the_v1_api() {
        let mock = Mock::start(Duration::ZERO, |_, _| reply(200, "{}")).await;
        let gateway = gateway(&mock, 300, TIMEOUT);
        for uri in [
            "/v1/curseforge/..%2Fv2%2Fmods",
            "/v1/curseforge/mods/1%3Fpage=2",
            "/v1/curseforge/mods//1",
            "/v1/curseforge/mods/1/",
        ] {
            let got = get(&gateway.router, uri).await;
            assert_eq!(got.status, StatusCode::BAD_REQUEST, "{uri}");
            assert_eq!(got.json()["code"], "INVALID_PATH", "{uri}");
        }
        assert_eq!(mock.calls(), 0);
    }

    #[tokio::test]
    async fn a_curseforge_that_does_not_answer_in_time_is_a_504() {
        let mock = Mock::start(Duration::from_millis(500), |_, _| reply(200, "{}")).await;
        let gateway = gateway(&mock, 300, Duration::from_millis(50));
        let got = get(&gateway.router, "/v1/curseforge/mods/1").await;
        assert_eq!(got.status, StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(got.json()["code"], "UPSTREAM_TIMEOUT");
        assert_eq!(
            gateway.state.budget.snapshot().await.days[0].pools[0]
                .counts
                .calls,
            1
        );
    }

    #[tokio::test]
    async fn needs_a_token() {
        let mock = Mock::start(Duration::ZERO, |_, _| reply(200, "{}")).await;
        let gateway = gateway(&mock, 300, TIMEOUT);
        let response = gateway
            .router
            .clone()
            .oneshot(
                Request::get("/v1/curseforge/mods/1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(mock.calls(), 0);
    }

    #[test]
    fn the_cache_table_matches_routes_by_shape() {
        let found =
            |path: &str| freshness(&path.split('/').collect::<Vec<_>>()).map(|f| f.found_ms);
        assert_eq!(found("mods/search"), Some(5 * MINUTE_MS));
        assert_eq!(found("mods/238222"), Some(HOUR_MS));
        assert_eq!(found("mods/238222/files/4567/download-url"), Some(HOUR_MS));
        assert_eq!(found("mods/238222/files/4567/changelog"), Some(DAY_MS));
        assert_eq!(found("minecraft/modloader"), Some(DAY_MS));
        assert_eq!(found("mods/jei"), None);
        assert_eq!(found("mods/238222/files/latest"), None);
        assert_eq!(found("games"), None);
    }
}
