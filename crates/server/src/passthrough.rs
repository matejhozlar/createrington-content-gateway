use std::sync::Arc;

use axum::{
    body::Bytes,
    http::{
        HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri,
        header::{CACHE_CONTROL, CONTENT_TYPE},
    },
    response::{IntoResponse, Response},
};

use crate::{
    app::AppState,
    auth::Pool,
    batch::{self, Batch},
    budget::{self, Refusal},
    cache::{Answer, AnswerCache, Entry, Finisher, LoadFailure, Outcome},
    curseforge::{self, Share},
    error::ApiError,
    modrinth,
    upstream::{Source, UpstreamFailure},
};

pub const MINUTE_MS: u64 = 60_000;
pub const HOUR_MS: u64 = 60 * MINUTE_MS;
pub const DAY_MS: u64 = 24 * HOUR_MS;
const X_CACHE: HeaderName = HeaderName::from_static("x-cache");

/// How long an answer of a route stays fresh, found and (when a 404 is kept) not found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Freshness {
    pub found_ms: u64,
    pub not_found_ms: Option<u64>,
}

pub const fn fresh(found_ms: u64, not_found_ms: Option<u64>) -> Option<Freshness> {
    Some(Freshness {
        found_ms,
        not_found_ms,
    })
}

/// A path segment `source` may be asked for: letters, digits, `-` and `_`, plus for Modrinth the other characters
/// its slugs may hold (`.`, `+`, `!`, `(`, `)`, `~`, `'`, `,`, `@`, `$`). Never `.` or `..` alone, nor anything that
/// could change the request, such as `/`, `%`, `?` or `#`.
pub fn valid_segment(source: Source, segment: &str) -> bool {
    let slug_byte = |byte: u8| source == Source::Modrinth && b".+!()~',@$".contains(&byte);
    !segment.is_empty()
        && segment != "."
        && segment != ".."
        && segment.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_' || slug_byte(byte)
        })
}

pub fn is_id(segment: &str) -> bool {
    !segment.is_empty() && segment.bytes().all(|b| b.is_ascii_digit())
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

pub fn wants_fresh(headers: &HeaderMap) -> bool {
    headers.get_all(CACHE_CONTROL).iter().any(|value| {
        value.to_str().is_ok_and(|value| {
            value
                .split(',')
                .any(|directive| directive.trim().eq_ignore_ascii_case("no-cache"))
        })
    })
}

/// One request to pass through: the path under the upstream's API, as the consumer sent it.
pub struct Request {
    pub path: String,
    pub pool: Pool,
    pub method: Method,
    pub uri: Uri,
    pub headers: HeaderMap,
    pub body: Bytes,
}

/// What `{method} {path}` of `source` answers, from the cache while it is fresh, as the CurseForge and Modrinth
/// passthroughs do.
pub async fn passthrough(state: Arc<AppState>, source: Source, request: Request) -> Response {
    let Request {
        path,
        pool,
        method,
        uri,
        headers,
        body,
    } = request;
    let segments: Vec<&str> = path.split('/').collect();
    if !segments
        .iter()
        .all(|segment| valid_segment(source, segment))
    {
        return ApiError::invalid_path().into_response();
    }
    let query = uri.query().map(str::to_owned);
    let forced = wants_fresh(&headers);
    let no_query = query.as_deref().is_none_or(str::is_empty);

    match (source, method.clone(), segments.as_slice()) {
        (Source::CurseForge, Method::POST, _) if no_query => {
            if let Some(batch) = Batch::of(&segments)
                && let Some(ids) = batch.ids(&body)
            {
                return batch::answer(state, pool, batch, ids, forced).await;
            }
        }
        (Source::Modrinth, Method::POST, ["version_files"])
            if no_query && modrinth::sha1_body(&body) =>
        {
            return modrinth::version_files(state, pool, &body, forced).await;
        }
        (Source::Modrinth, Method::GET, ["version_file", hash])
            if modrinth::sha1_query(query.as_deref()) =>
        {
            return modrinth::version_file(state, pool, hash, forced).await;
        }
        _ => {}
    }

    let freshness = match (method == Method::GET, source) {
        (true, Source::CurseForge) => curseforge::freshness(&segments),
        (true, Source::Modrinth) => modrinth::freshness(&segments),
        (false, _) => None,
    };
    let Some(freshness) = freshness else {
        let call = {
            let (state, pool) = (state.clone(), pool.clone());
            tokio::spawn(async move {
                call(&state, source, &pool, method, &path, query.as_deref(), body).await
            })
        };
        return match call.await {
            Ok(Ok(answer)) => respond(&answer, "miss"),
            Ok(Err(failure)) => failed(failure, &pool, source),
            Err(_) => failed(
                LoadFailure::Upstream(UpstreamFailure::Unreachable),
                &pool,
                source,
            ),
        };
    };

    let share = match source {
        Source::CurseForge => curseforge::share_of(&segments, query.as_deref()),
        Source::Modrinth => None,
    };
    let key = match share {
        Some(Share::Mod(project)) => Batch::Mods.key(project),
        _ => source.key(&cache_key(&path, query.as_deref())),
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
    let budget = state.budget_of(source);
    if !forced
        && let Some(Share::File { project, .. }) = share
        && let Some(entry) = shared_file.filter(|entry| {
            entry.is_fresh(now)
                && entry.answer.status == StatusCode::OK
                && entry.owner == Some(project)
        })
    {
        budget.record_hit(&pool.consumer, &pool.name);
        return respond(&entry.answer, "hit");
    }
    if !forced && let Some(entry) = cached.as_ref().filter(|entry| entry.is_fresh(now)) {
        budget.record_hit(&pool.consumer, &pool.name);
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
            source,
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
                budget.record_hit(&pool.consumer, &pool.name);
                respond(&entry.answer, "hit")
            }
        }
        Err(failure) => {
            let stale = cached.filter(|entry| entry.is_stale(state.clock.now_ms()));
            match stale {
                Some(entry) if batch::failed_upstream(&failure) => {
                    budget.record_hit(&pool.consumer, &pool.name);
                    respond(&entry.answer, "stale")
                }
                _ => failed(failure, &pool, source),
            }
        }
    }
}

struct Load {
    state: Arc<AppState>,
    source: Source,
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
            self.source,
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
    source: Source,
    pool: &Pool,
    method: Method,
    path: &str,
    query: Option<&str>,
    body: Bytes,
) -> Result<Answer, LoadFailure> {
    state
        .budget_of(source)
        .admit(&pool.consumer, &pool.name, 1)
        .await
        .map_err(LoadFailure::Refused)?;
    send_recorded(state, source, pool, method, path, query, body).await
}

/// Sends one call the budget of `source` already admitted and records it. A 429 comes back as the hold it starts.
pub async fn send_recorded(
    state: &AppState,
    source: Source,
    pool: &Pool,
    method: Method,
    path: &str,
    query: Option<&str>,
    body: Bytes,
) -> Result<Answer, LoadFailure> {
    let budget = state.budget_of(source);
    match state
        .upstream_of(source)
        .send(method, path, query, body)
        .await
    {
        Ok(sent) => {
            budget
                .record(
                    &pool.consumer,
                    &pool.name,
                    Some(sent.answer.status.as_u16()),
                    sent.retry_after.as_deref(),
                )
                .await;
            if sent.answer.status == StatusCode::TOO_MANY_REQUESTS {
                return Err(LoadFailure::Refused(Refusal::Hold {
                    retry_after_ms: budget::hold_ms(
                        sent.retry_after.as_deref(),
                        state.clock.now_ms(),
                    ),
                }));
            }
            Ok(sent.answer)
        }
        Err(failure) => {
            budget.record(&pool.consumer, &pool.name, None, None).await;
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

pub fn failed(failure: LoadFailure, pool: &Pool, source: Source) -> Response {
    match failure {
        LoadFailure::Answered(answer) => respond(&answer, "miss"),
        LoadFailure::Refused(refusal) => ApiError::refused(refusal, &pool.name).into_response(),
        LoadFailure::Upstream(failure) => {
            ApiError::upstream(failure, source.name()).into_response()
        }
    }
}
