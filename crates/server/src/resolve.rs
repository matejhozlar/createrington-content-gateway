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
mod tests {
    use axum::http::Request;
    use serde_json::{Value, json};

    use super::*;
    use crate::{
        app::Timeouts, config::DEFAULT_USER_AGENT, curseforge::TIMEOUT, modrinth, test_support::*,
    };

    const JEI_SHA1: &str = "3e1dd9fb5ea2f0dd4ffcc2b1a7d0bb1a1a9d6a8e";
    const SODIUM_SHA1: &str = "0123456789abcdef0123456789abcdef01234567";
    const CREATE_SHA1: &str = "abcdefabcdefabcdefabcdefabcdefabcdefabcd";

    fn file(id: u64, name: &str, size: u64, sha1: &str, url: Option<&str>) -> Value {
        json!({
            "id": id, "modId": id / 10, "fileName": name, "fileLength": size,
            "downloadUrl": url,
            "hashes": [{ "value": sha1.to_ascii_uppercase(), "algo": 1 }, { "value": "md5md5", "algo": 2 }]
        })
    }

    fn files_answer(files: &[Value]) -> String {
        json!({ "data": files }).to_string()
    }

    fn modrinth_answer(found: &[(&str, &str)]) -> String {
        let mut body = serde_json::Map::new();
        for (sha1, name) in found {
            body.insert(
                (*sha1).to_owned(),
                json!({ "id": "v", "files": [
                    { "hashes": { "sha1": sha1 }, "url": format!("https://cdn.modrinth.com/data/p/versions/v/{name}"), "filename": name, "size": 1 },
                    { "hashes": { "sha1": "ffffffffffffffffffffffffffffffffffffffff" }, "url": "https://cdn.modrinth.com/data/p/versions/v/other.jar", "filename": "other.jar", "size": 1 }
                ] }),
            );
        }
        Value::Object(body).to_string()
    }

    fn cdn_reply(status: u16, length: &'static str) -> Reply {
        Reply {
            status,
            headers: vec![("content-length", length)],
            body: String::new(),
        }
    }

    async fn resolve(gateway: &Gateway, body: &str) -> Got {
        post_json(&gateway.router, "/v1/resolve", body).await
    }

    #[tokio::test]
    async fn takes_curseforge_then_modrinth_then_the_cdn_and_says_when_nothing_has_a_file() {
        let mock = Mock::start(Duration::ZERO, |_, seen| match seen.uri.as_str() {
            "/v1/mods/files" => reply(
                200,
                files_answer(&[
                    file(
                        1001,
                        "jei.jar",
                        10,
                        JEI_SHA1,
                        Some("https://edge.forgecdn.net/jei.jar"),
                    ),
                    file(1002, "sodium.jar", 20, SODIUM_SHA1, None),
                    file(1003, "create 1.0(a).jar", 30, CREATE_SHA1, None),
                    file(
                        1004,
                        "gone.jar",
                        40,
                        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                        None,
                    ),
                ]),
            ),
            "/v2/version_files" => reply(200, modrinth_answer(&[(SODIUM_SHA1, "sodium.jar")])),
            "/files/1/3/create%201.0%28a%29.jar" => cdn_reply(200, "30"),
            "/files/1/4/gone.jar" => cdn_reply(403, "0"),
            uri => panic!("unexpected {uri}"),
        })
        .await;
        let gateway = gateway(&mock);

        let got = resolve(&gateway, r#"{"fileIds":[1001,1002,1003,1004,9999,1001]}"#).await;
        assert_eq!(got.status, StatusCode::OK);
        assert_eq!(got.header("x-cache"), None);
        let files = got.json()["files"].clone();
        assert_eq!(files.as_array().unwrap().len(), 5);
        assert_eq!(files[0]["source"], "curseforge");
        assert_eq!(files[0]["url"], "https://edge.forgecdn.net/jei.jar");
        assert_eq!(files[0]["sha1"], JEI_SHA1);
        assert_eq!(files[0]["fileName"], "jei.jar");
        assert_eq!(files[0]["size"], 10);
        assert_eq!(files[1]["source"], "modrinth");
        assert_eq!(
            files[1]["url"],
            "https://cdn.modrinth.com/data/p/versions/v/sodium.jar"
        );
        assert_eq!(files[2]["source"], "curseforge-cdn");
        assert_eq!(
            files[2]["url"],
            format!("{}/files/1/3/create%201.0%28a%29.jar", mock.base)
        );
        assert_eq!(files[3]["source"], Value::Null);
        assert_eq!(files[3]["url"], Value::Null);
        assert_eq!(files[3]["retry"], false);
        assert_eq!(files[4]["fileId"], 9999);
        assert_eq!(files[4]["source"], Value::Null);
        assert_eq!(files[4]["sha1"], Value::Null);
        assert_eq!(files[4]["retry"], false);

        let seen = mock.seen();
        let modrinth = seen
            .iter()
            .find(|seen| seen.uri == "/v2/version_files")
            .unwrap();
        let asked: Value = serde_json::from_str(&modrinth.body).unwrap();
        assert_eq!(
            asked["hashes"],
            json!([
                SODIUM_SHA1,
                CREATE_SHA1,
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            ])
        );
        let heads: Vec<&Seen> = seen.iter().filter(|seen| seen.method == "HEAD").collect();
        assert_eq!(heads.len(), 2);
        assert!(
            heads
                .iter()
                .all(|head| head.user_agent.as_deref() == Some(USER_AGENT))
        );
        assert_eq!(gateway.state.curseforge_budget.snapshot().await.used, 1);

        let again = resolve(&gateway, r#"{"fileIds":[1003,1004]}"#).await;
        assert_eq!(again.json()["files"][0]["source"], "curseforge-cdn");
        assert_eq!(mock.calls(), seen.len());
    }

    #[tokio::test]
    async fn the_cdn_is_skipped_when_asked_and_a_wrong_size_is_no_answer() {
        let mock = Mock::start(Duration::ZERO, |_, seen| match seen.uri.as_str() {
            "/v1/mods/files" => reply(
                200,
                files_answer(&[
                    file(1003, "create.jar", 30, CREATE_SHA1, None),
                    file(1005, "short.jar", 50, SODIUM_SHA1, None),
                ]),
            ),
            "/v2/version_files" => reply(200, "{}"),
            "/files/1/5/short.jar" => cdn_reply(200, "49"),
            uri => panic!("unexpected {uri}"),
        })
        .await;
        let gateway = gateway(&mock);

        let off = resolve(&gateway, r#"{"fileIds":[1003],"cdn":false}"#).await;
        assert_eq!(off.json()["files"][0]["source"], Value::Null);
        assert_eq!(off.json()["files"][0]["retry"], false);
        assert!(mock.seen().iter().all(|seen| seen.method != "HEAD"));

        let wrong = resolve(&gateway, r#"{"fileIds":[1005]}"#).await;
        assert_eq!(wrong.json()["files"][0]["source"], Value::Null);
        assert_eq!(wrong.json()["files"][0]["retry"], false);
        resolve(&gateway, r#"{"fileIds":[1005]}"#).await;
        assert_eq!(
            mock.seen()
                .iter()
                .filter(|seen| seen.method == "HEAD")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn a_modrinth_failure_asks_the_cdn_and_marks_the_rest_for_a_retry() {
        let mock = Mock::start(Duration::ZERO, |_, seen| match seen.uri.as_str() {
            "/v1/mods/files" => reply(
                200,
                files_answer(&[
                    file(1003, "create.jar", 30, CREATE_SHA1, None),
                    file(1006, "lost.jar", 60, SODIUM_SHA1, None),
                ]),
            ),
            "/v2/version_files" => reply(500, "{}"),
            "/files/1/3/create.jar" => cdn_reply(200, "30"),
            "/files/1/6/lost.jar" => cdn_reply(404, "0"),
            uri => panic!("unexpected {uri}"),
        })
        .await;
        let gateway = gateway(&mock);

        let got = resolve(&gateway, r#"{"fileIds":[1003,1006]}"#).await;
        assert_eq!(got.status, StatusCode::OK);
        assert_eq!(got.json()["files"][0]["source"], "curseforge-cdn");
        assert_eq!(got.json()["files"][0]["retry"], false);
        assert_eq!(got.json()["files"][1]["source"], Value::Null);
        assert_eq!(got.json()["files"][1]["retry"], true);
    }

    #[tokio::test]
    async fn a_curseforge_failure_is_the_passthrough_s_answer() {
        let mock = Mock::start(Duration::ZERO, |_, _| reply(500, "{}")).await;
        let failing = gateway(&mock);
        let got = resolve(&failing, r#"{"fileIds":[1003]}"#).await;
        assert_eq!(got.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(got.header("x-cache"), Some("miss"));

        let refused = gateway_with(
            &mock,
            GatewayWith {
                limit: 1,
                ..GatewayWith::default()
            },
        );
        resolve(&refused, r#"{"fileIds":[1003]}"#).await;
        let got = resolve(&refused, r#"{"fileIds":[1004]}"#).await;
        assert_eq!(got.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(got.json()["code"], "BUDGET_SPENT");
    }

    #[tokio::test]
    async fn refuses_a_bad_body() {
        let mock = Mock::start(Duration::ZERO, |_, _| reply(200, "{}")).await;
        let gateway = gateway(&mock);
        for body in ["{}", r#"{"fileIds":[]}"#, r#"{"fileIds":["x"]}"#, "nope"] {
            let got = resolve(&gateway, body).await;
            assert_eq!(got.status, StatusCode::BAD_REQUEST, "{body}");
            assert_eq!(got.json()["code"], "INVALID_BODY", "{body}");
        }
        let many: Vec<String> = (0..1001).map(|id| id.to_string()).collect();
        let got = resolve(&gateway, &format!(r#"{{"fileIds":[{}]}}"#, many.join(","))).await;
        assert_eq!(got.status, StatusCode::BAD_REQUEST);
        assert_eq!(got.json()["code"], "INVALID_BODY");
        assert_eq!(mock.calls(), 0);
    }

    #[test]
    fn the_longest_call_is_the_three_sources_in_a_row() {
        assert_eq!(
            TIMEOUT + modrinth::TIMEOUT + CDN_WAIT,
            Duration::from_secs(40)
        );
    }

    #[tokio::test]
    async fn a_file_without_a_name_or_size_keeps_its_link_and_skips_the_cdn() {
        let mock = Mock::start(Duration::ZERO, |_, seen| match seen.uri.as_str() {
            "/v1/mods/files" => reply(
                200,
                json!({ "data": [
                    { "id": 1007, "modId": 100, "downloadUrl": "https://edge.forgecdn.net/bare.jar", "hashes": [] },
                    { "id": 1008, "modId": 100, "fileLength": 80, "hashes": [{ "value": CREATE_SHA1, "algo": 1 }] },
                ] })
                .to_string(),
            ),
            "/v2/version_files" => reply(200, "{}"),
            uri => panic!("unexpected {uri}"),
        })
        .await;
        let gateway = gateway(&mock);

        let got = resolve(&gateway, r#"{"fileIds":[1007,1008]}"#).await;
        let files = got.json()["files"].clone();
        assert_eq!(files[0]["source"], "curseforge");
        assert_eq!(files[0]["fileName"], Value::Null);
        assert_eq!(files[0]["size"], Value::Null);
        assert_eq!(files[1]["source"], Value::Null);
        assert_eq!(files[1]["sha1"], CREATE_SHA1);
        assert_eq!(files[1]["size"], 80);
        assert_eq!(files[1]["retry"], false);
        assert!(mock.seen().iter().all(|seen| seen.method != "HEAD"));
    }

    #[test]
    fn builds_the_cdn_address_like_curseforge_does() {
        let cdn = Cdn::new(CURSEFORGE_CDN, DEFAULT_USER_AGENT, CDN_TIMEOUT, CDN_WAIT).unwrap();
        assert_eq!(
            cdn.url(4567890, "Create-1.20.1 (v0.5.1f)!'*.jar"),
            "https://mediafilez.forgecdn.net/files/4567/890/Create-1.20.1%20%28v0.5.1f%29%21%27%2A.jar"
        );
        assert_eq!(
            cdn.url(999, "ü.jar"),
            "https://mediafilez.forgecdn.net/files/0/999/%C3%BC.jar"
        );
    }

    fn only_on_the_cdn(head: Reply) -> impl Fn(usize, &Seen) -> Reply + Send + Sync + 'static {
        let (status, headers) = (head.status, head.headers);
        move |_, seen| match seen.uri.as_str() {
            "/v1/mods/files" => reply(
                200,
                files_answer(&[file(1003, "create.jar", 30, CREATE_SHA1, None)]),
            ),
            "/v2/version_files" => reply(200, "{}"),
            "/files/1/3/create.jar" => Reply {
                status,
                headers: headers.clone(),
                body: String::new(),
            },
            uri => panic!("unexpected {uri}"),
        }
    }

    fn heads(mock: &Mock) -> usize {
        mock.seen()
            .iter()
            .filter(|seen| seen.method == "HEAD")
            .count()
    }

    #[tokio::test]
    async fn a_curseforge_that_does_not_answer_in_time_is_a_504() {
        let (mock, _never) = Mock::held(only_on_the_cdn(cdn_reply(200, "30"))).await;
        let gateway = gateway_with(
            &mock,
            GatewayWith {
                timeouts: Timeouts {
                    curseforge: Duration::from_millis(50),
                    ..Timeouts::default()
                },
                ..GatewayWith::default()
            },
        );
        let got = resolve(&gateway, r#"{"fileIds":[1003]}"#).await;
        assert_eq!(got.status, StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(got.json()["code"], "UPSTREAM_TIMEOUT");
    }

    #[tokio::test]
    async fn a_cdn_error_asks_for_a_retry_and_is_not_kept() {
        let mock = Mock::start(Duration::ZERO, only_on_the_cdn(cdn_reply(503, "0"))).await;
        let gateway = gateway(&mock);
        for asked in 1..=2 {
            let got = resolve(&gateway, r#"{"fileIds":[1003]}"#).await;
            let file = &got.json()["files"][0];
            assert_eq!(file["source"], Value::Null);
            assert_eq!(file["retry"], true);
            assert_eq!(heads(&mock), asked);
        }
    }

    #[tokio::test]
    async fn a_cdn_that_does_not_answer_asks_for_a_retry() {
        let (mock, _never) = Mock::held_when(
            |seen| seen.method == "HEAD",
            only_on_the_cdn(cdn_reply(200, "30")),
        )
        .await;
        let gateway = gateway_with(
            &mock,
            GatewayWith {
                timeouts: Timeouts {
                    cdn: Duration::from_millis(50),
                    ..Timeouts::default()
                },
                ..GatewayWith::default()
            },
        );
        let got = resolve(&gateway, r#"{"fileIds":[1003]}"#).await;
        let file = &got.json()["files"][0];
        assert_eq!(file["source"], Value::Null);
        assert_eq!(file["retry"], true);
    }

    #[tokio::test]
    async fn a_check_of_the_same_file_joins_the_one_running() {
        let (mock, gate) = Mock::held_when(
            |seen| seen.method == "HEAD",
            only_on_the_cdn(cdn_reply(200, "30")),
        )
        .await;
        let gateway = gateway(&mock);
        let first = tokio::spawn({
            let router = gateway.router.clone();
            async move { post_json(&router, "/v1/resolve", r#"{"fileIds":[1003]}"#).await }
        });
        until("the first check to reach the CDN", || heads(&mock) == 1).await;
        let second = tokio::spawn({
            let router = gateway.router.clone();
            async move { post_json(&router, "/v1/resolve", r#"{"fileIds":[1003]}"#).await }
        });
        until("the second check to join it", || {
            gateway.state.cache.joined() == 1
        })
        .await;
        gate.open();

        for got in [first.await.unwrap(), second.await.unwrap()] {
            let file = &got.json()["files"][0];
            assert_eq!(file["source"], "curseforge-cdn");
            assert_eq!(file["retry"], false);
        }
        assert_eq!(heads(&mock), 1);
    }

    #[test]
    fn a_kept_cdn_verdict_reads_back_as_it_was_given() {
        for verdict in [
            CdnVerdict::Found("https://mediafilez.forgecdn.net/files/1/3/a.jar".to_owned()),
            CdnVerdict::Missing,
        ] {
            let entry = verdict.entry(NOV_3_2026_20H).unwrap();
            assert_eq!(CdnVerdict::kept(&entry), verdict);
        }
        assert!(CdnVerdict::Unknown.entry(NOV_3_2026_20H).is_none());
    }

    #[tokio::test]
    async fn a_check_outlives_the_request_that_started_it_and_is_kept_for_the_retry() {
        let (mock, gate) = Mock::held_when(
            |seen| seen.method == "HEAD",
            only_on_the_cdn(cdn_reply(200, "30")),
        )
        .await;
        let gateway = gateway_with(
            &mock,
            GatewayWith {
                timeouts: Timeouts {
                    cdn_wait: Duration::from_millis(50),
                    ..Timeouts::default()
                },
                ..GatewayWith::default()
            },
        );
        let gave_up = resolve(&gateway, r#"{"fileIds":[1003]}"#).await;
        assert_eq!(gave_up.json()["files"][0]["retry"], true);

        gate.open();
        until("the check to finish", || {
            gateway.state.cache.loads_running() == 0
        })
        .await;
        let retried = resolve(&gateway, r#"{"fileIds":[1003]}"#).await;
        let file = &retried.json()["files"][0];
        assert_eq!(file["source"], "curseforge-cdn");
        assert_eq!(file["retry"], false);
        assert_eq!(heads(&mock), 1);
    }

    #[tokio::test]
    async fn checks_still_running_when_the_cdn_wait_ends_ask_for_a_retry() {
        let (mock, _never) = Mock::held_when(
            |seen| seen.method == "HEAD",
            only_on_the_cdn(cdn_reply(200, "30")),
        )
        .await;
        let gateway = gateway_with(
            &mock,
            GatewayWith {
                timeouts: Timeouts {
                    cdn_wait: Duration::from_millis(50),
                    ..Timeouts::default()
                },
                ..GatewayWith::default()
            },
        );
        let got = tokio::time::timeout(
            Timeouts::default().cdn / 2,
            resolve(&gateway, r#"{"fileIds":[1003]}"#),
        )
        .await
        .expect("the wait ended the check before the CDN client gave up");
        assert_eq!(got.status, StatusCode::OK);
        let file = &got.json()["files"][0];
        assert_eq!(file["source"], Value::Null);
        assert_eq!(file["retry"], true);
    }

    #[tokio::test]
    async fn a_modrinth_link_on_another_host_is_not_given_out() {
        let mock = Mock::start(Duration::ZERO, |_, seen| match seen.uri.as_str() {
            "/v1/mods/files" => reply(
                200,
                files_answer(&[file(1003, "create.jar", 30, CREATE_SHA1, None)]),
            ),
            "/v2/version_files" => reply(
                200,
                json!({ CREATE_SHA1: { "id": "v", "files": [
                    { "hashes": { "sha1": CREATE_SHA1 }, "url": "https://elsewhere.example.com/create.jar", "filename": "create.jar", "size": 30 },
                    { "hashes": { "sha1": CREATE_SHA1 }, "url": "http://cdn.modrinth.com/data/p/versions/v/create.jar", "filename": "create.jar", "size": 30 }
                ] } })
                .to_string(),
            ),
            "/files/1/3/create.jar" => cdn_reply(200, "30"),
            uri => panic!("unexpected {uri}"),
        })
        .await;
        let gateway = gateway(&mock);
        let got = resolve(&gateway, r#"{"fileIds":[1003]}"#).await;
        let file = &got.json()["files"][0];
        assert_eq!(file["source"], "curseforge-cdn");
        assert_eq!(file["url"], format!("{}/files/1/3/create.jar", mock.base));
    }

    #[tokio::test]
    async fn no_cache_asks_every_source_again() {
        let mock = Mock::start(Duration::ZERO, only_on_the_cdn(cdn_reply(200, "30"))).await;
        let gateway = gateway(&mock);
        resolve(&gateway, r#"{"fileIds":[1003]}"#).await;
        let kept = resolve(&gateway, r#"{"fileIds":[1003]}"#).await;
        assert_eq!(kept.json()["files"][0]["source"], "curseforge-cdn");
        assert_eq!(mock.calls(), 3);

        let forced = send(
            &gateway.router,
            Request::post("/v1/resolve")
                .header("content-type", "application/json")
                .header("cache-control", "no-cache"),
            r#"{"fileIds":[1003]}"#,
        )
        .await;
        assert_eq!(forced.json()["files"][0]["source"], "curseforge-cdn");
        let asked: Vec<String> = mock.seen()[3..]
            .iter()
            .map(|seen| format!("{} {}", seen.method, seen.uri))
            .collect();
        assert_eq!(
            asked,
            [
                "POST /v1/mods/files",
                "POST /v2/version_files",
                "HEAD /files/1/3/create.jar"
            ]
        );
    }
}
