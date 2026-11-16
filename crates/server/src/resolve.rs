use std::{collections::HashMap, sync::Arc, time::Duration};

use axum::{
    Json,
    body::Bytes,
    extract::{State, rejection::JsonRejection},
    http::{HeaderMap, HeaderValue, StatusCode, header::CONTENT_LENGTH},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use utoipa::ToSchema;

use crate::{
    app::AppState,
    auth::Pool,
    batch::Batch,
    cache::{Answer, AnswerCache, Entry, LoadFailure, Outcome, load_key},
    clock::{HOUR_MS, MINUTE_MS},
    error::ApiError,
    load,
    modrinth::VersionFiles,
    passthrough,
    upstream::Source,
};

pub const CURSEFORGE_CDN: &str = "https://mediafilez.forgecdn.net";
pub const CDN_TIMEOUT: Duration = Duration::from_secs(10);
pub const CDN_WAIT: Duration = Duration::from_secs(20);
const CDN_AT_ONCE: usize = 8;
const CDN_FOUND_MS: u64 = HOUR_MS;
const CDN_NOT_FOUND_MS: u64 = 10 * MINUTE_MS;
const MODRINTH_CDN_HOST: &str = "cdn.modrinth.com";
const SHA1_ALGO: u8 = 1;
const MOST_FILES: usize = 1000;

/// The client that checks built addresses on CurseForge's CDN with HEAD requests.
pub struct Cdn {
    client: reqwest::Client,
    base: String,
    wait: Duration,
}

impl Cdn {
    pub fn new(
        base: impl Into<String>,
        user_agent: &str,
        timeout: Duration,
        wait: Duration,
    ) -> reqwest::Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(timeout)
                .user_agent(user_agent)
                .build()?,
            base: base.into(),
            wait,
        })
    }

    fn url(&self, file_id: u64, file_name: &str) -> String {
        format!(
            "{}/files/{}/{}/{}",
            self.base,
            file_id / 1000,
            file_id % 1000,
            encoded(file_name)
        )
    }

    async fn head(&self, url: &str) -> Result<(StatusCode, Option<u64>), reqwest::Error> {
        let response = self.client.head(url).send().await?;
        let length = response
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok()?.parse().ok());
        Ok((response.status(), length))
    }
}

fn encoded(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The body of `POST /v1/resolve`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ResolveRequest {
    /// CurseForge file ids, up to 1000.
    #[schema(example = json!([4567890, 4567891]))]
    pub file_ids: Vec<u64>,
    /// Whether a file's built address on CurseForge's CDN may be checked and given when neither CurseForge nor
    /// Modrinth has a link. On unless set to false.
    #[serde(default = "yes")]
    #[schema(default = true)]
    pub cdn: bool,
}

fn yes() -> bool {
    true
}

/// Where a file's download address comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum FileSource {
    Curseforge,
    Modrinth,
    CurseforgeCdn,
}

/// What `POST /v1/resolve` answers for each file asked for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedFile {
    pub file_id: u64,
    /// Where `url` comes from, or null when no source has the file.
    pub source: Option<FileSource>,
    /// The address to download the file from, or null when no source has it.
    pub url: Option<String>,
    /// The file's SHA-1 as CurseForge lists it, null when CurseForge does not know the file.
    pub sha1: Option<String>,
    /// The file's name as CurseForge lists it, null when CurseForge does not know the file.
    pub file_name: Option<String>,
    /// The file's size in bytes as CurseForge lists it, null when CurseForge does not know the file.
    pub size: Option<u64>,
    /// True when a source could not be asked for the file, so that a null `source` is worth asking again later
    /// rather than remembering.
    pub retry: bool,
}

/// What `POST /v1/resolve` answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct Resolved {
    /// One entry per file id asked for, in the order asked, each id once.
    pub files: Vec<ResolvedFile>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileRow {
    file_name: Option<String>,
    file_length: Option<u64>,
    download_url: Option<String>,
    #[serde(default)]
    hashes: Vec<FileHash>,
}

#[derive(Deserialize)]
struct FileHash {
    value: String,
    algo: u8,
}

#[derive(Deserialize)]
struct Version {
    #[serde(default)]
    files: Vec<VersionFile>,
}

#[derive(Deserialize)]
struct VersionFile {
    hashes: VersionHashes,
    url: String,
}

#[derive(Deserialize)]
struct VersionHashes {
    sha1: Option<String>,
}

struct ListedFile {
    file_name: Option<String>,
    size: Option<u64>,
    sha1: Option<String>,
    download_url: Option<String>,
}

struct Candidate {
    file_id: u64,
    file_name: String,
    size: u64,
}

/// Modrinth's links by SHA-1, or that Modrinth could not be asked.
enum ModrinthLinks {
    Asked(HashMap<String, String>),
    Unasked,
}

impl ModrinthLinks {
    fn of(&self, file: &ListedFile) -> Option<&String> {
        match self {
            Self::Asked(links) => links.get(file.sha1.as_ref()?),
            Self::Unasked => None,
        }
    }

    fn could_have(&self, file: &ListedFile) -> bool {
        matches!(self, Self::Unasked) && file.sha1.is_some()
    }
}

/// What CurseForge's CDN said about a file's built address: there, not there, or not asked in time.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CdnVerdict {
    Found(String),
    Missing,
    Unknown,
}

impl CdnVerdict {
    fn kept(entry: &Entry) -> Self {
        if entry.answer.status != StatusCode::OK {
            return Self::Missing;
        }
        String::from_utf8(entry.answer.body.to_vec()).map_or(Self::Missing, Self::Found)
    }

    fn entry(&self, now_ms: u64) -> Option<Entry> {
        let (answer, fresh_ms) = match self {
            Self::Found(url) => (
                Answer {
                    status: StatusCode::OK,
                    content_type: Some(HeaderValue::from_static("text/plain")),
                    body: Bytes::from(url.clone()),
                },
                CDN_FOUND_MS,
            ),
            Self::Missing => (Answer::not_found(), CDN_NOT_FOUND_MS),
            Self::Unknown => return None,
        };
        Some(Entry::new(answer, now_ms, fresh_ms))
    }
}

/// A download address for each CurseForge file.
///
/// The address is CurseForge's own link, else Modrinth's copy of the file found by its SHA-1, else the file's built
/// address on CurseForge's CDN when a HEAD answers 200 with the file's size.
///
/// The files are read from the cache like `POST /v1/curseforge/mods/files`, and the CurseForge calls for the ones
/// not kept count against the budget. Modrinth answers and CDN verdicts are kept for an hour, misses for 10
/// minutes. No bytes pass through the gateway: the consumer downloads from the address given. A null `source` with
/// `retry` false means no source has the file; `retry` true means a source could not be asked this time. At worst
/// a call takes 40 s: 10 s for CurseForge, 10 s for Modrinth and 20 s for the CDN checks.
#[utoipa::path(
    post,
    path = "/v1/resolve",
    tag = "gateway",
    security(("bearer" = [])),
    params(
        ("X-Budget-Pool" = Option<String>, Header, description = "The pool the CurseForge calls draw from. Required when the token has more than one pool"),
        ("Cache-Control" = Option<String>, Header, description = "`no-cache` asks every source again instead of answering from the cache"),
    ),
    request_body(content = ResolveRequest, content_type = "application/json"),
    responses(
        (status = 200, description = "An address per file, in the order asked", body = Resolved),
        (status = 400, description = "`INVALID_BODY`, `POOL_REQUIRED` or `UNKNOWN_POOL`", body = ApiError),
        (status = 401, description = "No valid consumer token", body = ApiError),
        (status = 413, description = "The files need more CurseForge calls than a whole minute allows, `REQUEST_TOO_LARGE`; split them", body = ApiError),
        (status = 500, description = "The gateway failed to finish the request: `INTERNAL`", body = ApiError),
        (status = 502, description = "CurseForge could not be reached or answered something the gateway cannot keep: `UPSTREAM_UNREACHABLE`, `UPSTREAM_TOO_LARGE` or `UPSTREAM_UNREADABLE`", body = ApiError),
        (status = 503, description = "The CurseForge call was refused: `BUDGET_SPENT` this minute or `UPSTREAM_HOLD` after a 429", body = ApiError, headers(("Retry-After" = u64, description = "Seconds until the next call can go out"))),
        (status = 504, description = "CurseForge did not answer in time, `UPSTREAM_TIMEOUT`", body = ApiError),
        (status = "default", description = "Any other status CurseForge answered the file lookup with, with its body as it came, marked `X-Cache`. Only the gateway's own answers lack `X-Cache`"),
    )
)]
pub async fn resolve(
    State(state): State<Arc<AppState>>,
    pool: Pool,
    headers: HeaderMap,
    body: Result<Json<ResolveRequest>, JsonRejection>,
) -> Response {
    let asked = match requested_ids(body) {
        Ok(asked) => asked,
        Err(error) => return error.into_response(),
    };
    let forced = passthrough::wants_fresh(&headers);
    let ids = asked.file_ids;
    let known = match known_files(&state, &pool, &ids, forced).await {
        Ok(known) => known,
        Err(failure) => return passthrough::failed(failure, &pool, Source::CurseForge),
    };
    let unlinked: Vec<(u64, &ListedFile)> = ids
        .iter()
        .filter_map(|id| Some((*id, known.get(id)?)))
        .filter(|(_, file)| file.download_url.is_none())
        .collect();
    let modrinth = modrinth_links(&state, &pool, &unlinked, forced).await;
    let on_cdn = if asked.cdn {
        let candidates: Vec<Candidate> = unlinked
            .iter()
            .filter_map(|(id, file)| cdn_candidate(*id, file, &modrinth))
            .collect();
        Some(cdn_links(&state, candidates, forced).await)
    } else {
        None
    };
    Json(assemble(&ids, &known, &modrinth, on_cdn.as_ref())).into_response()
}

struct Asked {
    file_ids: Vec<u64>,
    cdn: bool,
}

fn requested_ids(body: Result<Json<ResolveRequest>, JsonRejection>) -> Result<Asked, ApiError> {
    let Ok(Json(request)) = body else {
        return Err(ApiError::invalid_body(
            "a JSON object with `fileIds`, a list of CurseForge file ids",
        ));
    };
    if request.file_ids.is_empty() {
        return Err(ApiError::invalid_body(
            "`fileIds` must list at least one file id",
        ));
    }
    let file_ids = load::distinct(request.file_ids, MOST_FILES).ok_or_else(|| {
        ApiError::invalid_body(&format!("`fileIds` may list at most {MOST_FILES} file ids"))
    })?;
    Ok(Asked {
        file_ids,
        cdn: request.cdn,
    })
}

async fn known_files(
    state: &Arc<AppState>,
    pool: &Pool,
    ids: &[u64],
    forced: bool,
) -> Result<HashMap<u64, ListedFile>, LoadFailure> {
    let rows = load::rows(state, pool, Batch::Files, ids.to_vec(), forced)
        .await?
        .value;
    Ok(rows
        .iter()
        .filter_map(|(id, row)| {
            let row: FileRow = serde_json::from_slice(row)
                .inspect_err(|error| {
                    tracing::warn!(file_id = id, %error, "CurseForge's row of a file does not read as a file");
                })
                .ok()?;
            Some((
                *id,
                ListedFile {
                    file_name: row.file_name,
                    size: row.file_length,
                    sha1: row
                        .hashes
                        .iter()
                        .find(|hash| hash.algo == SHA1_ALGO)
                        .and_then(|hash| passthrough::sha1_of(&hash.value)),
                    download_url: row.download_url,
                },
            ))
        })
        .collect())
}

async fn modrinth_links(
    state: &Arc<AppState>,
    pool: &Pool,
    unlinked: &[(u64, &ListedFile)],
    forced: bool,
) -> ModrinthLinks {
    let hashes: Vec<String> = unlinked
        .iter()
        .filter_map(|(_, file)| file.sha1.clone())
        .collect();
    if hashes.is_empty() {
        return ModrinthLinks::Asked(HashMap::new());
    }
    match load::rows(state, pool, VersionFiles, hashes, forced).await {
        Ok(given) => ModrinthLinks::Asked(
            given
                .value
                .into_iter()
                .filter_map(|(hash, version)| {
                    let url = modrinth_url(&hash, &version)?;
                    Some((hash, url))
                })
                .collect(),
        ),
        Err(failure) => {
            tracing::warn!(
                files = unlinked.len(),
                %failure,
                "Modrinth could not be asked for the files"
            );
            ModrinthLinks::Unasked
        }
    }
}

fn is_cdn_candidate(file: &ListedFile, modrinth: &ModrinthLinks) -> bool {
    file.download_url.is_none()
        && modrinth.of(file).is_none()
        && file.file_name.is_some()
        && file.size.is_some()
}

fn cdn_candidate(file_id: u64, file: &ListedFile, modrinth: &ModrinthLinks) -> Option<Candidate> {
    if !is_cdn_candidate(file, modrinth) {
        return None;
    }
    Some(Candidate {
        file_id,
        file_name: file.file_name.clone()?,
        size: file.size?,
    })
}

fn assemble(
    ids: &[u64],
    known: &HashMap<u64, ListedFile>,
    modrinth: &ModrinthLinks,
    on_cdn: Option<&HashMap<u64, CdnVerdict>>,
) -> Resolved {
    let files = ids
        .iter()
        .map(|id| {
            let Some(file) = known.get(id) else {
                return ResolvedFile {
                    file_id: *id,
                    source: None,
                    url: None,
                    sha1: None,
                    file_name: None,
                    size: None,
                    retry: false,
                };
            };
            let (found, retry) = if let Some(url) = &file.download_url {
                (Some((FileSource::Curseforge, url.clone())), false)
            } else if let Some(url) = modrinth.of(file) {
                (Some((FileSource::Modrinth, url.clone())), false)
            } else {
                let verdict = match on_cdn {
                    Some(on_cdn) if is_cdn_candidate(file, modrinth) => {
                        on_cdn.get(id).cloned().unwrap_or(CdnVerdict::Unknown)
                    }
                    _ => CdnVerdict::Missing,
                };
                match verdict {
                    CdnVerdict::Found(url) => (Some((FileSource::CurseforgeCdn, url)), false),
                    CdnVerdict::Missing => (None, modrinth.could_have(file)),
                    CdnVerdict::Unknown => (None, true),
                }
            };
            let (source, url) =
                found.map_or((None, None), |(source, url)| (Some(source), Some(url)));
            ResolvedFile {
                file_id: *id,
                source,
                url,
                sha1: file.sha1.clone(),
                file_name: file.file_name.clone(),
                size: file.size,
                retry,
            }
        })
        .collect();
    Resolved { files }
}

fn modrinth_url(hash: &str, version: &[u8]) -> Option<String> {
    let version: Version = serde_json::from_slice(version).ok()?;
    version
        .files
        .into_iter()
        .find(|file| {
            file.hashes
                .sha1
                .as_ref()
                .is_some_and(|sha1| sha1.eq_ignore_ascii_case(hash))
                && is_modrinth_cdn(&file.url)
        })
        .map(|file| file.url)
}

fn is_modrinth_cdn(url: &str) -> bool {
    reqwest::Url::parse(url)
        .is_ok_and(|url| url.scheme() == "https" && url.host_str() == Some(MODRINTH_CDN_HOST))
}

fn cdn_key(file_id: u64, size: u64) -> String {
    format!("cdn:{file_id}:{size}")
}

async fn cdn_links(
    state: &Arc<AppState>,
    candidates: Vec<Candidate>,
    forced: bool,
) -> HashMap<u64, CdnVerdict> {
    let mut verdicts = HashMap::with_capacity(candidates.len());
    if candidates.is_empty() {
        return verdicts;
    }
    let keys: Vec<String> = candidates
        .iter()
        .map(|file| cdn_key(file.file_id, file.size))
        .collect();
    let now = state.clock.now_ms();
    let kept = state.cache.lookup_many(&keys, now).await;
    let at_once = Arc::new(Semaphore::new(CDN_AT_ONCE));
    let mut waits = Vec::new();
    for ((file, key), kept) in candidates.into_iter().zip(keys).zip(kept) {
        if let Some(entry) = kept.filter(|entry| !forced && entry.is_fresh(now)) {
            verdicts.insert(file.file_id, CdnVerdict::kept(&entry));
            continue;
        }
        let file_id = file.file_id;
        let load_key = load_key(key.clone(), forced);
        let (waiting, finisher) = state.cache.join(&load_key);
        if let Some(finisher) = finisher {
            let (state, at_once, checked) = (state.clone(), at_once.clone(), key.clone());
            let check = async move {
                let now = state.clock.now_ms();
                let kept = state
                    .cache
                    .get(&key)
                    .filter(|entry| !forced && entry.is_fresh(now));
                let outcome = match kept {
                    Some(entry) => Outcome::not_called(Ok(entry)),
                    None => {
                        let _slot = at_once.acquire().await;
                        let verdict = head(&state, &file).await;
                        Outcome::called(match verdict.entry(state.clock.now_ms()) {
                            Some(entry) => {
                                let entry = Arc::new(entry);
                                state.cache.insert(&key, entry.clone());
                                Ok(entry)
                            }
                            None => Err(LoadFailure::Lost),
                        })
                    }
                };
                state.cache.finish(&load_key, finisher, outcome);
            };
            load::spawn_load(check, checked, 1);
        }
        waits.push((file_id, waiting));
    }
    let collect = async {
        for (file_id, waiting) in waits {
            let verdict = match AnswerCache::wait(waiting).await {
                Some(Outcome {
                    loaded: Ok(entry), ..
                }) => CdnVerdict::kept(&entry),
                _ => CdnVerdict::Unknown,
            };
            verdicts.insert(file_id, verdict);
        }
    };
    let wait = state.cdn.wait;
    if tokio::time::timeout(wait, collect).await.is_err() {
        tracing::warn!(
            wait_ms = wait.as_millis(),
            "CurseForge's CDN could not be asked about every file in time"
        );
    }
    verdicts
}

async fn head(state: &AppState, file: &Candidate) -> CdnVerdict {
    let file_id = file.file_id;
    let url = state.cdn.url(file_id, &file.file_name);
    match state.cdn.head(&url).await {
        Ok((StatusCode::OK, Some(length))) if length == file.size => CdnVerdict::Found(url),
        Ok((StatusCode::OK, length)) => {
            tracing::warn!(
                file_id,
                cdn_bytes = ?length,
                api_bytes = file.size,
                "CurseForge's CDN and API list different sizes for a file"
            );
            CdnVerdict::Missing
        }
        Ok((StatusCode::FORBIDDEN | StatusCode::NOT_FOUND, _)) => CdnVerdict::Missing,
        Ok((status, _)) => {
            tracing::warn!(file_id, %status, "CurseForge's CDN answered a file with an unexpected status");
            CdnVerdict::Unknown
        }
        Err(error) => {
            tracing::warn!(file_id, %error, "CurseForge's CDN could not be asked about a file");
            CdnVerdict::Unknown
        }
    }
}

#[cfg(test)]
mod tests;
